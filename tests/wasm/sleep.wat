;; Preview 1 module that never returns. Epoch interruption stops it: wasmtime
;; checks the epoch on the loop back-edge, which is what `docker stop` bumps.
(module
  (memory (export "memory") 1)
  (func (export "_start")
    (loop $forever
      (br $forever)))
)
