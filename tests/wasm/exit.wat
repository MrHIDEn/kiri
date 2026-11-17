;; Preview 1 module: exit 42, so the engine can report that code.
(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (call $proc_exit (i32.const 42)))
)
