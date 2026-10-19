// Boots the Linux seat VM with libkrun (Hypervisor.framework). libkrun rather
// than Virtualization.framework because its balloon does free page reporting,
// so memory the guest frees goes back to macOS. The EFI build boots its
// bundled edk2, which starts systemd-boot from the guest's own disk.
//
//   linux-vm-launcher --cpus N --mem MiB [--disk PATH[:ro]]...
//     [--net-unixgram PATH] [--vsock PORT:PATH:listen|connect]...
//     [--console FILE] [--log-level 0-5]
//     [--topology BIG,BIGCAP,LITTLECAP,BIGQOS,LITTLEQOS]
//     [--virtiofs TAG:PATH:DAX_MIB]...
//
// Disks attach in order as vda, vdb, ...; `listen` makes the guest listen on
// PORT with the host socket at PATH, `connect` lets the guest reach a host
// listener at PATH.
#include <errno.h>
#include <libkrun.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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
