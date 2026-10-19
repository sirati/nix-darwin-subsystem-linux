[
  ./topology.patch
  # Both upstream bugs in macOS virtio-fs DAX, needed for shared memory.
  ./dax-worker.patch
  ./dax-readonly.patch
  # Reopening a virtio-serial port (the bells) panicked the VM.
  ./console-reopen.patch
  # A DAX mapping of a file tagged user.krun.shm maps that shm object: shared
  # memory that is freed when the host unlinks it. Also makes the host-only
  # attributes really host-only (the old check never matched).
  ./shm-dax.patch
]
