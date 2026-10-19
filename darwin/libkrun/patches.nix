[
  ./topology.patch
  # Both upstream bugs in macOS virtio-fs DAX, needed for shared memory.
  ./dax-worker.patch
  ./dax-readonly.patch
  # Reopening a virtio-serial port (the bells) panicked the VM.
  ./console-reopen.patch
]
