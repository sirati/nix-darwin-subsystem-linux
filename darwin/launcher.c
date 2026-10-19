// Boots the Linux seat VM with libkrun (Hypervisor.framework). libkrun rather
// than Virtualization.framework because its balloon does free page reporting,
// so memory the guest frees goes back to macOS. The EFI build boots its
// bundled edk2, which starts systemd-boot from the guest's own disk.
//
//   linux-vm-launcher --cpus N --mem MiB [--disk PATH[:ro]]...
//     [--net-unixgram PATH] [--vsock PORT:PATH:listen|connect]...
//     [--console FILE] [--log-level 0-5]
//     [--topology BIG,BIGCAP,LITTLECAP,BIGQOS,LITTLEQOS]
//     [--virtiofs TAG:PATH:DAX_MIB]... [--bell NAME:PREFIX]...
//     [--shm-file PATH:NAME:MIB]...
//
// Disks attach in order as vda, vdb, ...; `listen` makes the guest listen on
// PORT with the host socket at PATH, `connect` lets the guest reach a host
// listener at PATH.
#include <errno.h>
#include <fcntl.h>
#include <libkrun.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>

static void check(int32_t ret, const char *what) {
  if (ret < 0) {
    fprintf(stderr, "linux-vm-launcher: %s: %s\n", what, strerror(-ret));
    exit(1);
  }
}

static const char *need(int argc, char **argv, int *i) {
  if (*i + 1 >= argc) {
    fprintf(stderr, "linux-vm-launcher: %s needs a value\n", argv[*i]);
    exit(2);
  }
  return argv[++*i];
}

int main(int argc, char **argv) {
  long cpus = 0, mem = 0;
  // Logging is process-wide and must be set before the context exists.
  for (int i = 1; i + 1 < argc; i++)
    if (!strcmp(argv[i], "--log-level"))
      check(krun_set_log_level((uint32_t)strtoul(argv[i + 1], NULL, 10)), "log level");
  int ctx = krun_create_ctx();
  check(ctx, "create context");
  // A stock guest kernel has no TSI, so plain vsock without hijacking.
  check(krun_disable_implicit_vsock(ctx), "disable implicit vsock");
  check(krun_add_vsock(ctx, 0), "add vsock");
  int disks = 0;
  int bells = -1;
  for (int i = 1; i < argc; i++) {
    const char *arg = argv[i];
    if (!strcmp(arg, "--cpus")) {
      cpus = strtol(need(argc, argv, &i), NULL, 10);
    } else if (!strcmp(arg, "--mem")) {
      mem = strtol(need(argc, argv, &i), NULL, 10);
    } else if (!strcmp(arg, "--log-level")) {
      need(argc, argv, &i);
    } else if (!strcmp(arg, "--disk")) {
      char *spec = strdup(need(argc, argv, &i));
      size_t len = strlen(spec);
      bool ro = len > 3 && !strcmp(spec + len - 3, ":ro");
      if (ro) spec[len - 3] = '\0';
      char id[16];
      snprintf(id, sizeof id, "vd%c", 'a' + disks++);
      check(krun_add_disk2(ctx, id, spec, KRUN_DISK_FORMAT_RAW, ro), "add disk");
    } else if (!strcmp(arg, "--net-unixgram")) {
      // Fixed MAC so gvproxy's DHCP lease stays the same across boots.
      uint8_t mac[6] = {0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee};
      check(krun_add_net_unixgram(ctx, need(argc, argv, &i), -1, mac, COMPAT_NET_FEATURES,
                                  NET_FLAG_VFKIT),
            "add network");
    } else if (!strcmp(arg, "--vsock")) {
      char *spec = strdup(need(argc, argv, &i));
      char *path = strchr(spec, ':');
      char *mode = strrchr(spec, ':');
      if (!path || path == mode) {
        fprintf(stderr, "linux-vm-launcher: --vsock wants PORT:PATH:listen|connect\n");
        return 2;
      }
      *path++ = '\0';
      *mode++ = '\0';
      bool listen = !strcmp(mode, "listen");
      if (!listen && strcmp(mode, "connect")) {
        fprintf(stderr, "linux-vm-launcher: vsock mode must be listen or connect\n");
        return 2;
      }
      check(krun_add_vsock_port2(ctx, (uint32_t)strtoul(spec, NULL, 10), path, listen),
            "add vsock port");
    } else if (!strcmp(arg, "--topology")) {
      // BIG,BIGCAP,LITTLECAP,BIGQOS,LITTLEQOS (./libkrun-topology.patch).
      unsigned big, bigcap, littlecap, bigqos, littleqos;
      if (sscanf(need(argc, argv, &i), "%u,%u,%u,%u,%u", &big, &bigcap, &littlecap, &bigqos,
                 &littleqos) != 5) {
        fprintf(stderr, "linux-vm-launcher: --topology wants BIG,BIGCAP,LITTLECAP,BIGQOS,LITTLEQOS\n");
        return 2;
      }
      check(krun_set_cpu_topology(ctx, (uint8_t)big, bigcap, littlecap, bigqos, littleqos),
            "cpu topology");
    } else if (!strcmp(arg, "--virtiofs")) {
      // TAG:PATH:DAX_MIB. With a DAX window the guest maps the host files'
      // pages directly, so host and guest share memory through a file.
      char *spec = strdup(need(argc, argv, &i));
      char *path = strchr(spec, ':');
      char *size = strrchr(spec, ':');
      if (!path || path == size) {
        fprintf(stderr, "linux-vm-launcher: --virtiofs wants TAG:PATH:DAX_MIB\n");
        return 2;
      }
      *path++ = '\0';
      *size++ = '\0';
      check(krun_add_virtiofs2(ctx, spec, path, strtoull(size, NULL, 10) << 20), "add virtiofs");
    } else if (!strcmp(arg, "--bell")) {
      // NAME:PREFIX. A named virtio-serial port (/dev/virtio-ports/NAME in
      // the guest) for tiny wake bytes. Its Mac ends are FIFOs, PREFIX.to-guest
      // and PREFIX.from-guest, held open read-write here so whatever serves
      // them can come and go without the port seeing EOF.
      char *spec = strdup(need(argc, argv, &i));
      char *prefix = strchr(spec, ':');
      if (!prefix) {
        fprintf(stderr, "linux-vm-launcher: --bell wants NAME:PREFIX\n");
        return 2;
      }
      *prefix++ = '\0';
      if (bells < 0) {
        bells = krun_add_virtio_console_multiport(ctx);
        check(bells, "add bell console");
        // Port 0 of a multiport device is a console tty (hvcN); the bells
        // want raw ports, so it gets an idle pipe and they start at port 1.
        int idle[2];
        if (pipe(idle)) {
          perror("linux-vm-launcher: pipe");
          return 1;
        }
        check(krun_add_console_port_inout(ctx, bells, "", idle[0], open("/dev/null", O_WRONLY)),
              "add idle console port");
      }
      int fds[2];
      const char *dirs[2] = {"to-guest", "from-guest"};
      for (int d = 0; d < 2; d++) {
        char path[1024];
        snprintf(path, sizeof path, "%s.%s", prefix, dirs[d]);
        if (mkfifo(path, 0600) && errno != EEXIST) {
          fprintf(stderr, "linux-vm-launcher: mkfifo %s: %s\n", path, strerror(errno));
          return 1;
        }
        fds[d] = open(path, O_RDWR);
        if (fds[d] < 0) {
          fprintf(stderr, "linux-vm-launcher: open %s: %s\n", path, strerror(errno));
          return 1;
        }
      }
      check(krun_add_console_port_inout(ctx, bells, spec, fds[0], fds[1]), "add bell port");
    } else if (!strcmp(arg, "--shm-file")) {
      // PATH:NAME:MIB. Shared memory, not a file: the POSIX shared memory
      // object NAME, made here if missing (macOS sizes one only once, so an
      // existing one keeps its size), and PATH a sparse file of its size
      // tagged user.krun.shm=NAME. A DAX mapping of PATH maps the object
      // (./libkrun/shm-dax.patch), and Mac processes map it by the tag; its
      // pages are touched lazily and freed when it is unlinked.
      char *spec = strdup(need(argc, argv, &i));
      char *name = strchr(spec, ':');
      char *size = strrchr(spec, ':');
      if (!name || name == size) {
        fprintf(stderr, "linux-vm-launcher: --shm-file wants PATH:NAME:MIB\n");
        return 2;
      }
      *name++ = '\0';
      *size++ = '\0';
      off_t want = (off_t)strtoull(size, NULL, 10) << 20;
      int shm = shm_open(name, O_CREAT | O_RDWR, 0600);
      struct stat st;
      if (shm < 0 || fstat(shm, &st)) {
        fprintf(stderr, "linux-vm-launcher: shm_open %s: %s\n", name, strerror(errno));
        return 1;
      }
      if (st.st_size == 0 && ftruncate(shm, want)) {
        fprintf(stderr, "linux-vm-launcher: size %s: %s\n", name, strerror(errno));
        return 1;
      }
      if (st.st_size != 0 && st.st_size != want)
        fprintf(stderr, "linux-vm-launcher: %s keeps its size, %lld MiB\n", name,
                (long long)(st.st_size >> 20));
      off_t have = st.st_size ? st.st_size : want;
      close(shm);
      int tag = open(spec, O_CREAT | O_RDWR, 0600);
      if (tag < 0 || ftruncate(tag, have) ||
          fsetxattr(tag, "user.krun.shm", name, strlen(name), 0, 0)) {
        fprintf(stderr, "linux-vm-launcher: tag %s: %s\n", spec, strerror(errno));
        return 1;
      }
      close(tag);
    } else if (!strcmp(arg, "--console")) {
      check(krun_set_console_output(ctx, need(argc, argv, &i)), "console output");
    } else {
      fprintf(stderr, "linux-vm-launcher: unknown argument %s\n", arg);
      return 2;
    }
  }
  if (cpus <= 0 || mem <= 0 || disks == 0) {
    fprintf(stderr, "linux-vm-launcher: --cpus, --mem and a --disk are required\n");
    return 2;
  }
  check(krun_set_vm_config(ctx, (uint8_t)cpus, (uint32_t)mem), "vm config");
  // Only returns on failure; the VM's exit ends the process.
  check(krun_start_enter(ctx), "start");
  return 1;
}
