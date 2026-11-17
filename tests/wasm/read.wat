;; Preview 1 module: read note.txt from the first preopened directory and try
;; to create nope.txt there. A read-only preopen prints " readonly".
(module
  (import "wasi_snapshot_preview1" "fd_prestat_get" (func $fd_prestat_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 300) "note.txt")
  (data (i32.const 320) "nope.txt")
  (data (i32.const 340) " readonly\n")
  (func (export "_start")
    (local $fd i32)
    (local $errno i32)
    (local $n i32)
    (local.set $fd (i32.const 3))
    (block $found
      (loop $scan
        (br_if $found (i32.eqz (call $fd_prestat_get (local.get $fd) (i32.const 0))))
        (local.set $fd (i32.add (local.get $fd) (i32.const 1)))
        (br_if $scan (i32.lt_u (local.get $fd) (i32.const 32)))
        (call $proc_exit (i32.const 1))))
    ;; path_open note.txt, rights FD_READ|FD_SEEK (6). The opened fd lands at 40.
    (local.set $errno (call $path_open
      (local.get $fd) (i32.const 0) (i32.const 300) (i32.const 8)
      (i32.const 0) (i64.const 6) (i64.const 0) (i32.const 0) (i32.const 40)))
    (if (i32.ne (local.get $errno) (i32.const 0))
      (then (call $proc_exit (i32.const 1))))
    (i32.store (i32.const 48) (i32.const 64))
    (i32.store (i32.const 52) (i32.const 64))
    (drop (call $fd_read (i32.load (i32.const 40)) (i32.const 48) (i32.const 1) (i32.const 56)))
    (local.set $n (i32.load (i32.const 56)))
    (i32.store (i32.const 48) (i32.const 64))
    (i32.store (i32.const 52) (local.get $n))
    (drop (call $fd_write (i32.const 1) (i32.const 48) (i32.const 1) (i32.const 60)))
    ;; oflags CREAT (1), rights FD_WRITE (64). Any errno means the directory is read-only.
    (local.set $errno (call $path_open
      (local.get $fd) (i32.const 0) (i32.const 320) (i32.const 8)
      (i32.const 1) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 40)))
    (if (i32.eq (local.get $errno) (i32.const 0))
      (then
        (i32.store8 (i32.const 80) (i32.const 120))
        (i32.store (i32.const 48) (i32.const 80))
        (i32.store (i32.const 52) (i32.const 1))
        (local.set $errno (call $fd_write (i32.load (i32.const 40)) (i32.const 48) (i32.const 1) (i32.const 60)))))
    (if (i32.ne (local.get $errno) (i32.const 0))
      (then
        (i32.store (i32.const 48) (i32.const 340))
        (i32.store (i32.const 52) (i32.const 10))
        (drop (call $fd_write (i32.const 1) (i32.const 48) (i32.const 1) (i32.const 60)))))
    (call $proc_exit (i32.const 0)))
)
