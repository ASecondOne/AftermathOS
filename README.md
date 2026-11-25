# Aftermath

> **Status: experimental.** Development consoles and authenticated login work in QEMU.
> Expect breakage, don't install it on hardware you care about.

Aftermath is an Arch-based Linux distribution with a **custom Rust init instead
of systemd as PID 1**. It can reuse standalone components such as udev, and it
ships **Bedrock Linux-style strata**:
several complete userlands living side by side, managed by a separate strata
service that runs next to the normal Arch boot.

**Development rule:** new Aftermath implementation, test helpers and build
automation are Rust. Existing Linux, PAM, udev, D-Bus and Plasma components
may be reused; those components have not been rewritten in Rust. See `AGENTS.md`.

## Vision

1. **Rust init (`aftermath-init`)** – a systemd-like service manager without
   systemd: dependency-ordered services, cgroups, supervision, clean
   shutdown. Long term it answers the D-Bus APIs desktops expect
   (`org.freedesktop.login1`, `org.freedesktop.systemd1`, `hostname1`, ...),
   so KDE Plasma and friends work without systemd/logind.
2. **Strata** – prepackaged strata under `/aftermath/strata/<name>`, in the
   spirit of Bedrock Linux. The Arch userland you boot is the *host* stratum;
   the strata service makes programs from other strata available alongside it,
   without replacing the normal Arch boot.
3. **Windows stratum** (the big plan) – a native Windows toolchain/runtime
   running as a stratum. Similar in goal to Wine, but not Wine. Kernel support
   it may need (`binfmt_misc`, namespaces, FUSE) is already enabled.
4. **Custom desktop** – developed in parallel (separate project).

## Architecture (current state)

```
 firmware (BIOS/UEFI)
   └─ GRUB with the Aftermath OS theme (disk image or ISO)  ── or ──  QEMU -kernel/-initrd (--direct)
        └─ Linux 7.4-rc3 from ./linux  (config: defconfig + kvm_guest + config/kernel/aftermath.config)
             └─ small initramfs = static Rust bootstrap/init + private BusyBox
                  └─ rdinit=/usr/bin/init → aftermath-init (PID 1, tiny, never panics)
                       ├─ aftermath-early: disk-backed SquashFS + writable overlay → /
                       ├─ mounts /proc /sys /dev /run /tmp, cgroup2; sets hostname
                       ├─ reaps zombies; poweroff/reboot/halt on signal
                       └─ supervises /usr/lib/aftermath/aftermath-engine
                            ├─ machine identity + foreground system D-Bus with readiness/health probes
                            ├─ foreground udev: readiness, coldplug, hotplug, bounded restarts
                            ├─ network: lo + dhcpcd
                            ├─ aftermath-strata setup: strata disk → /aftermath/strata/ubuntu,
                            │    shared mounts, cross launchers  ── fail → plain Arch host mode
                            ├─ development root consoles (ttyS0 + tty1)
                            ├─ authenticated login on tty2; Rust PAM session/runtime management
                            └─ marks the boot good in GRUB's grubenv
                       (engine keeps crashing → host mode → BusyBox rescue, see "Fallback layers")
```

The rootfs is built with `pacstrap` from `config/packages.x86_64`. That list
deliberately avoids Arch's `base` meta package, because `base` pulls in
systemd. (Some packages still need `systemd-libs`, which is only a library.)

The bootstrap is about 2 MiB. The full root lives in
`rootfs-aftermath.squashfs` on the boot disk or ISO, read on demand rather than
unpacked into RAM. Direct QEMU boot attaches that image as a read-only disk.
Early Rust code checks the matching build ID and mounts the root before handing
over to normal init. Missing or damaged root images enter independent BusyBox
rescue. No mkinitcpio or generated boot scripts are involved.

## Strata: Arch + Ubuntu side by side (Bedrock style)

Aftermath ships two strata: **arch** (the host, i.e. the booted rootfs `/`)
and **ubuntu** (Ubuntu 24.04 LTS "noble", in `/aftermath/strata/ubuntu`).

**Add Fedora while running** (root, in normal/full mode):

```sh
amctl list                         # installed, integrated, state and boot setting
amctl enable fedora                # download if missing, integrate now, enable at boot
dnf --version                      # Fedora's package manager, from the same shell
amctl strata enter fedora          # Fedora bash
amctl disable fedora               # detach integration; keep installed packages
```

The default shipment stays Arch + Ubuntu. Fedora is optional and initially off.
`amctl enable fedora` (also `amctl strata enable fedora`) downloads the official
[Fedora base image](https://fedoraproject.org/misc/) from
`quay.io/fedora/fedora:44` using the host's `skopeo`, then unpacks it with `umoci`.
There is no container daemon: the extracted userland becomes a normal stratum.
The image is **pinned by digest** (`FEDORA_IMAGE` in
`crates/aftermath-strata/src/catalog.rs`, the multi-arch index of `fedora:44`
as of 2026-11-25): the download goes over TLS and every manifest and layer must
match that digest. There is **no publisher signature check** (no sigstore
policy). To move to a newer Fedora image, look up the tag's current digest and
update the constant; inside the stratum, `dnf upgrade` updates packages as usual.

`amctl enable` **needs the running engine** (it asks the engine first, then
downloads, then has the engine start the stratum). In BusyBox rescue or when
the engine is down it says so and changes nothing; `amctl list` still works.
Both tools are included in `config/packages.x86_64`; when updating an older
development image, run `cargo am rootfs --update` and rebuild the initramfs.

Installation needs network access and free space on the writable strata disk
(`AFTERMATH_STRATA`); allow space for both the downloaded image and its unpacked
root. Private staging is kept on that disk, validated and renamed into
`/aftermath/store/fedora` only on success. Failed transfers are cleaned up, and
retrying the command is safe. An existing installation is never overwritten or
downloaded again. A hard kill or power loss may leave a hidden `.install-fedora-*`
staging directory; it is not an installed stratum. The installer serializes
downloads, and the engine keeps supervising services during the transfer.

**No reboot is needed in full mode.** Enable bind-mounts Fedora, shares users,
home directories and DNS, and refreshes command launchers, man-page and desktop
views immediately. `dnf`, `dnf5`, `yum` and RPM commands are pinned to Fedora;
package-manager changes refresh integration too. Existing engine shells clear
their command cache at each prompt; other shells may need `hash -r`.
This starts the userland integration, not a second init/service manager.
The boot switch is saved only after successful activation. Installed packages
persist on the strata disk; the enable/disable switch needs the persistent host
overlay to survive reboot. Explicit Arch-only/rescue mode still requires booting
normally before enabling strata.

`amctl list` includes configured optional strata that are not installed yet, so
`INSTALLED` and `INTEGRATED` distinguish available, stopped and running userlands.
Only Fedora has an automatic runtime downloader currently; Ubuntu remains
prebuilt and Windows remains a placeholder.
From the same shell both package managers work:

```sh
pacman -Syu                     # Arch (host)
makepkg -si                     # Arch (as a normal user, needs base-devel)
apt update && apt install htop  # Ubuntu
apt-get, apt-cache, dpkg, dpkg-query, add-apt-repository ppa:foo/bar
htop                            # not in Arch -> runs from Ubuntu automatically
strat ubuntu ls /               # explicitly run a command in a stratum
strat arch pacman -Q            # ... or in the host
strat --list                    # strata and their state
strat --which apt               # -> ubuntu
```

**How it works**

* **Storage: a separate strata disk.** Strata use their own writable
  **ext4 disk** (`out/strata.img`,
  label `AFTERMATH_STRATA`), writable and persistent. `cargo am run`
  attaches it as a second virtio disk.
* **`aftermath-strata setup`** (`crates/aftermath-strata`), run by the engine
  at boot:
  1. finds the disk by its ext4 label and mounts it on `/aftermath/store`
  2. binds `/aftermath/store/ubuntu` to `/aftermath/strata/ubuntu`
  3. shares `/proc /sys /dev /run /tmp /home /root` into the stratum
     (recursive bind, slave propagation)
  4. shares `/etc` pieces: the host `/etc` is bound read-only at
     `/run/aftermath/host-etc`; the stratum's `resolv.conf` and `hosts` are
     symlinks into it, `hostname` is copied, and the host's real users (root
     and uid/gid 1000-59999) are merged into the stratum's
     `passwd/shadow/group/gshadow`. (Not bind-mounted: `adduser` & co replace
     those files with `rename()`, which fails on a bind mount; Ubuntu keeps
     its own system users like `_apt`.)
  5. generates the cross launchers (below) and overlays the strata's man
     pages / `.desktop` files at `/aftermath/cross/share/{man,applications}`
     (`XDG_DATA_DIRS` includes `/aftermath/cross/share`).
* **Cross launchers.** `/aftermath/cross/pin/` and `/aftermath/cross/bin/`
  contain symlinks to one multicall binary, `strat`
  (`/usr/lib/aftermath/strat`). Called as `apt-get`, it looks the name up in
  `/aftermath/cross/index`, chroots into the owning stratum and execs it,
  keeping the current directory (shared dirs have the same path everywhere),
  the environment and the uid; only `PATH` becomes the stratum's default.
  The shell's PATH is
  `/aftermath/cross/pin:/usr/local/sbin:/usr/local/bin:/usr/bin:/aftermath/cross/bin`:
  `pin` holds commands pinned to another stratum that the host also has, `bin`
  holds commands only other strata have, so the host wins otherwise.
  After `apt`, `apt-get`, `dpkg` (or another package manager) run as root
  through strat, it calls `aftermath-strata refresh`, so `apt install htop &&
  htop` works right away; otherwise run `/usr/lib/aftermath/aftermath-strata refresh`.
  `strat` is setuid root so normal users can chroot; it drops back to the
  caller's uid/gid before running anything.
* **Precedence** is set in `/etc/aftermath/strata.toml`: `[cross.pin]` maps
  command names or globs to a stratum (`pacman`/`makepkg` → arch,
  `apt`, `apt-*`, `dpkg`, `dpkg-*`, `debconf*`, `add-apt-repository` → ubuntu;
  exact names beat globs), `[cross] order` decides the rest (host first).
  Your own choices (`amctl default`, below) beat both.
* **Man pages**: `man` is not in the Arch rootfs, so `man` resolves to
  Ubuntu's (`man-db` is in the stratum) and shows Ubuntu's pages; once the host
  has `man-db`, use `man -M /aftermath/cross/share/man <page>` for strata pages
  (Arch's `/etc/profile` unsets `MANPATH`).

### Which stratum runs a command: `amctl which` and `amctl default`

When a command exists in more than one stratum (`tac`, `ls`, `bash`, `vim`, ...),
`amctl which` lists **every copy**, with its real path, and marks which one
runs and why:

```text
$ amctl which tac
AFTERMATH which tac
  * arch      /usr/bin/tac
    ubuntu    /usr/bin/tac  (/aftermath/strata/ubuntu/usr/bin/tac)

  winner      arch  the host wins when nothing is pinned
  runs via    the host's PATH (no launcher for `tac`)
  unchecked   windows (disabled)
```

* `path` is where it sits in the stratum's PATH, `-> real` follows symlinks
  *inside* the stratum (`/usr/bin/vi -> /usr/bin/vim.basic` through
  `/etc/alternatives`), and the part in brackets is the same file as the host
  sees it. A copy hidden by an earlier one in the same stratum's PATH is marked
  "shadowed".
* The reason is one of: **your default** (`amctl default`), a **shipped pin**
  (`[cross.pin]` in strata.toml, with the pattern), the **lookup order**, or
  **the host** (nothing pinned, the host comes first). A default or pin whose
  stratum lacks the command is skipped (and shown as "skipped"), so the command
  never disappears.
* Works for every user and without the engine (it only reads files);
  `amctl --json which tac` for scripts; exit status 1 if no stratum has it.
* `strat --which -a tac` prints the same as plain text (`strat --which tac`
  still prints just the stratum name).

Choose the winner yourself (root; through the engine, or directly when it is
down):

```sh
amctl default tac ubuntu        # Ubuntu's tac from now on (beats shipped pins)
amctl default dpkg arch         # back to the host, against the shipped `dpkg = "ubuntu"` pin
amctl default tac --reset       # forget it: the shipped rules apply again
amctl default list              # your defaults, the lookup order, the shipped pins
amctl default order ubuntu arch # order for commands nothing pins (--reset to undo)
```

* Your choices live in their own file, **`/etc/aftermath/strata.d/user.toml`**
  (`[pin]` command = stratum, optional `order`); the shipped `strata.toml` is
  never touched. Only exact command names, no globs. A broken user.toml is
  ignored with a warning (plan B: the shipped rules).
* **No reboot**: the launchers are regenerated right away
  (`aftermath-strata refresh`). If strata are not up (Arch host mode), the
  choice is saved and applies when they are.
* **bash's command cache**: bash remembers where it found a command (`hash`),
  so an open shell would keep running the old copy. Shells started by the
  engine get `PROMPT_COMMAND="hash -r"`, so they pick up a change at the next
  prompt. In other shells (a desktop terminal) run `hash -r` once.
* It is in /etc, so it is kept across reboots with the host overlay disk and
  lost on a RAM-only root (like `state.toml`).
* A command literally called `list` or `order` can't be given a default this
  way (those words are subcommands); edit user.toml for that.

### pacman inside the VM

* **Network**: aftermath-engine starts `dhcpcd -b -q --nodev`. `--nodev` matters:
  dhcpcd is built with udev support and otherwise waits for udev to
  "initialise" each interface. It remains a fallback when udev fails or is
  disabled. Device discovery runs before DHCP; udev's naming rules may rename
  `eth0` to a name such as `enp0s2`.
* **Wait for the network before the prompt**: the engine holds the consoles
  back until there is a default route and a nameserver (usually ~1-3 s, at
  most 15 s; plan B: after that the shells start anyway and dhcpcd keeps
  trying). Without it, `apt update` typed right at the prompt ran before DHCP
  finished. `aftermath.netwait=<seconds>` on the kernel command line changes
  the limit (`0` = don't wait).

### apt inside the VM

* At every boot `aftermath-strata` writes
  `/etc/apt/apt.conf.d/99aftermath` into each Debian/Ubuntu stratum with
  `Acquire::Retries::Delay "false";`, and a `policy-rc.d` that refuses to
  start services if one is missing. apt 2.7 (noble) has a retry-queue bug:
  when the first try of a download fails, the delayed retry loops and prints
  thousands of `W: Tried to start delayed item ... InRelease, but failed`.
  Retrying right away avoids it. A failed download now shows apt's normal
  one-line `Err:`/`W: Failed to fetch ...` messages instead.
* **Landlock**: pacman 7 downloads as the unprivileged `alpm` user inside a
  Landlock sandbox and aborts every download if the kernel has no Landlock, so
  the kernel fragment enables `CONFIG_SECURITY_LANDLOCK`. (On an older kernel:
  `pacman --disable-sandbox-filesystem -Syu`.) `cargo am kernel` / `cargo am
  run` now rebuild the kernel config automatically when
  `config/kernel/aftermath.config` changes.
* **Saved across reboots** with the host overlay disk (see *Persistent host
  root*). pacman's package cache lives on the strata disk
  (`/aftermath/store/host/pacman-pkg`) so downloads don't eat RAM. For the
  image itself: add packages to `config/packages.x86_64` and `cargo am rootfs --update`.

**Plan B.** No strata disk, a broken disk or no stratum coming up →
`aftermath-strata` exits non-zero and the engine continues in **plain Arch host
mode** (logged as `strata FAILED ... -> continuing in plain Arch host mode`,
banner `[Arch host mode: strata disabled]`). One broken stratum doesn't stop
the others (`strat --list` shows `failed: <reason>`). In the BusyBox rescue
layer the strata disk is mounted read-only under `/mnt/<dev>` like every other
disk.

**Building the Ubuntu stratum** (Tim, needs sudo + network, ~300 MB download):

```sh
sudo pacman -S --needed debootstrap ubuntu-keyring
cargo am stratum ubuntu          # sudo debootstrap noble -> out/strata/ubuntu, then out/strata.img
cargo am run                     # attaches out/strata.img automatically
```

| Command | What it does | Root? |
|---|---|---|
| `cargo am stratum ubuntu [--suite noble] [--mirror URL] [--force] [--minimal] [--size 8G]` | **Stage 1:** `debootstrap --variant=minbase --include=ubuntu-keyring,apt-utils,ca-certificates,gnupg` (debootstrap's own resolver can't handle virtual packages, so nothing fancy here). Writes deb822 `ubuntu.sources` (main restricted universe multiverse, -updates, -backports, -security) and `policy-rc.d` (never start services). **Stage 2:** chroot with `/proc /sys /dev /dev/pts` mounted (always unmounted again), `apt-get update && apt-get install -y --no-install-recommends software-properties-common less nano man-db`. Then builds the strata disk. A half-finished tree is detected and deleted automatically; a finished debootstrap resumes at stage 2; `--force` rebuilds from scratch | **yes** |
| `cargo am stratum image [--size 8G]` | Rebuilds `out/strata.img` from `out/strata/` (`mkfs.ext4 -d`, sparse file) | **yes** |
| `cargo am stratum list` | Built strata, their release, what strata.toml says, disk usage | no |
| `cargo am stratum remove <name>` | `sudo rm -rf --one-file-system out/strata/<name>`, rebuilds the disk | **yes** |
| `cargo am run --no-overlay` / `--overlay-snapshot` | use a temporary writable layer / throw away new changes | no |
| `cargo am run --no-strata` / `--strata-snapshot` | boot without the strata disk (host mode) / throw away writes to it | no |
| `cargo am clean --strata` | delete `out/strata` and `out/strata.img` | **yes** |

Note: `cargo am stratum image` rebuilds the disk from `out/strata/`, which
throws away what you `apt install`ed inside the VM. The ISO has no strata disk
(it boots in Arch host mode unless a disk labelled `AFTERMATH_STRATA` is
attached).

**Not supported (yet):**

* **snap**: skipped on purpose. snapd needs systemd (it is driven by systemd
  units and talks to systemd over D-Bus), AppArmor confinement and squashfs
  loop mounts managed by systemd. None of that exists here; use apt, PPAs or
  Flatpak-style tools instead.
* **makepkg** refuses to run as root; create a normal user and install
  `base-devel` (`pacman -S base-devel`). Inside Ubuntu, services from packages
  are not started (no init in the stratum); apt prints a harmless note.
* **Cross libraries** (linking an Arch program against Ubuntu libraries) are
  not shared, by design: each stratum runs with its own `/usr`.
* Login shells started other ways than the engine's console shells (e.g. `su -`)
  get Arch's default PATH; prefix commands with `strat <stratum>` there or add
  the cross dirs to PATH.

## Persistent host root (saved changes) and `cargo am reset-vm`

The SquashFS image stays a clean, read-only base on disk. A small extra disk,
`out/host-overlay.img` (2G sparse, ext4 label `AFTERMATH_HOST`, created by
`cargo am run` without root), holds everything you change in the VM:

```
 aftermath-init (PID 1) ──exec──▶ aftermath-early (same PID, before anything else)
     finds the AFTERMATH_HOST disk, mounts it, then
     overlayfs:  lower = SquashFS (disk, read-only)   upper = <disk>/upper
     moves the overlay onto / (switch_root style) ──exec──▶ aftermath-init again
     no usable upper ──▶ temporary writable layer over the same SquashFS
     no usable image ──▶ independent bootstrap BusyBox rescue
```

* `pacman -Syu`, installed packages, `/root`, `/etc` edits survive reboots.
  The console banner says which one you got: *Changes to the system are saved*
  or *Temporary writable root: changes are lost at poweroff*.
* The disk stays mounted at `/aftermath/hostdisk` (`upper/` = your changes).
* Paths the image owns are removed from the saved changes at every boot
  (`/usr/lib/aftermath`, `/usr/bin/aftermath-init`, `/usr/lib/modules`,
  `/boot`, `/etc/aftermath/{version,build-id}`), so a rebuilt image always
  brings its own init, engine and strata service.
* Every image has a build id; when the image was rebuilt under existing saved
  changes, aftermath-early logs a warning (packages you upgraded in the VM
  shadow the image's newer ones; if that bites: "clean image" or `reset-vm`).
* **Plan B**: missing/broken overlay disk or saved changes hiding the boot
  programs → a temporary writable layer over the clean SquashFS. The GRUB
  **"clean image, ignore saved changes"** entry (`aftermath.overlay=off`)
  deliberately chooses that layer. It leaves the saved disk untouched.
* **BusyBox rescue** skips SquashFS entirely, using the bootstrap's static
  tools. Data partitions are mounted read-only under `/mnt/<dev>` for repairs.
  An unavailable or mismatched root image also enters this rescue.
* RAM contains the small bootstrap, active program pages and filesystem cache.
  Temporary writes use tmpfs; persistent writes use the overlay disk.

```sh
cargo am run --no-overlay          # temporary writable layer, nothing saved
cargo am run --overlay-snapshot    # use the saved changes, throw new ones away
cargo am reset-vm                  # fresh VM (asks first): see below
cargo am reset-vm -n               # only show what it would do
cargo am reset-vm --yes --keep-strata --keep-rootfs   # fast reset, no downloads
```

`cargo am reset-vm` deletes the overlay disk and VM state (`out/vm/OVMF_VARS`,
`disk.qcow2`), saves the session logs from the strata disk into
`out/vm/logs/session-<time>/` (`--logs` deletes all logs instead), rebuilds the
strata (`--keep-strata`: re-image the existing Ubuntu tree instead of a new
debootstrap), recreates the rootfs with pacstrap (`--keep-rootfs` skips that),
repacks the initramfs, rebuilds the GRUB disk and creates an empty overlay disk.
It needs sudo for the rootfs/strata/initramfs steps and asks before doing
anything (`--yes` skips the question).

## Bootloader: GRUB with the "Aftermath OS" menu

Aftermath ships GRUB with its own theme. The colours are always **red,
purple and black**: black background, red title and glow, purple accents. The menu has:

| Entry | What it does |
|---|---|
| **Aftermath OS** | normal boot (root shell on screen and on serial) |
| Aftermath OS (serial console) | `/dev/console` is the serial port |
| Aftermath OS (Arch host only, no strata) | `aftermath.mode=host`: fallback layer 2 |
| Aftermath OS (clean image, ignore saved changes) | `aftermath.overlay=off`: boots the pristine image, the host overlay disk is not used |
| Aftermath OS (rescue: BusyBox CLI) | `aftermath.mode=busybox`: fallback layer 3 |
| Aftermath OS (debug: verbose kernel log) | `earlyprintk`, `ignore_loglevel`, `loglevel=8` |
| UEFI Firmware Settings | only shown when booted via UEFI (`fwsetup`) |
| Reboot / Power off | |

Everything is generated by the build tool, no image editor needed:
`config/grub/themes/aftermath/theme.txt` is the layout; the background and
highlight images are drawn in Rust (`scripts/src/grub.rs`, `png` crate); the
fonts are made from DejaVu with `grub-mkfont`; `grub.cfg` is generated too.

The same GRUB setup is used by:

* the **boot disk image** `out/aftermath-<version>-x86_64.img` (what
  `cargo am run` boots by default). It is built **without sudo**: one
  FAT32 partition written with mtools (`EFI/BOOT/BOOTX64.EFI` for UEFI,
  GRUB `boot.img` + `core.img` in the MBR for BIOS), holding `/boot/grub`,
  the kernel, small initramfs and SquashFS root image.
* the **ISO** (`grub-mkrescue`, BIOS + UEFI).
* the **rootfs**: `grub` and `efibootmgr` are in `config/packages.x86_64`,
  and `/boot/grub` (config + theme + fonts) plus `/boot/vmlinuz-aftermath`
  are copied in, ready for a future installer.

## Fallback layers ("always a plan B")

Aftermath is built so that something always boots. Each layer falls back to
the next one by itself:

| Layer | What runs | Falls back when |
|---|---|---|
| 1. **Full** | `aftermath-engine --mode full`: strata + services + shells | strata fail → engine keeps going in host mode (logged); engine crashes 3x → layer 2 |
| 2. **Arch host** | `aftermath-engine --mode host`: plain Arch, no strata | engine crashes 3x → layer 3 |
| 3. **BusyBox rescue** | static BusyBox shell on every console (`/usr/lib/aftermath/rescue/busybox`), all partitions mounted **read-only** under `/mnt/<dev>` | – |
| before layer 1 | aftermath-early mounts SquashFS and the writable upper layer | bad upper → temporary layer; missing/mismatched root image → bootstrap BusyBox |
| last resort | if PID 1 itself panics, it catches the panic and goes to layer 3 | – |

How the pieces fit:

* **PID 1 (`aftermath-init`) is tiny on purpose.** It mounts `/proc`,
  `/sys`, `/dev`, ..., starts and supervises the engine, reaps processes and
  handles poweroff/reboot. Its main loop runs inside `catch_unwind` and the
  release profile uses `panic = "unwind"`, so a bug becomes a fallback
  instead of a kernel panic. When the engine dies, PID 1 kills what the
  engine left behind and restarts it (an engine that ran for 60 s or more
  resets the counter).
* **`aftermath-engine`** (`crates/aftermath-engine`) holds the real logic:
  strata (`/etc/aftermath/strata.toml`, `/usr/lib/aftermath/aftermath-strata`), the
  console shells, and marking the boot as good.
* **Kernel panics reboot**: every entry boots with `panic=5`.
* **GRUB boot counting** (disk image / installed systems, not the read-only
  ISO): GRUB sets `boot_success=0` in `/boot/grub/grubenv` only after the
  selected kernel and initramfs have loaded, immediately before launching Linux.
  Rebooting, powering off or entering firmware settings from the menu does not
  count as a failed boot. Each unconfirmed Linux attempt is counted once on
  return to GRUB; further menu visits preserve that count. Once the engine is up
  it sets `boot_success=1 boot_fails=0` (it finds the FAT
  boot partition itself and rewrites the 1024-byte block in place, like GRUB
  does). If a boot never gets that far:
  * 1 failed boot → try **Aftermath OS** again
  * 2–3 failed boots → default becomes **Arch host only**
  * 4+ failed boots → default becomes **rescue: BusyBox CLI**
  * **The counting window**: a boot counts as failed if anything stops it
    between GRUB launching Linux and the engine marking it good, which happens
    once the consoles are up (after strata and the network wait, so up to
    ~15 s more without a network). Powering off or resetting the VM in that
    window, even on purpose, counts as one failed boot. Two in a row make the
    next boot Arch host only; the next good boot resets the counter.
  * **The boot partition must be writable by GRUB**: GRUB's `save_env` only
    writes grubenv on plain filesystems such as FAT (the disk image) or ext4.
    Keep `/boot/grub` on one of those on installed systems; on btrfs, LVM,
    RAID or encrypted `/boot` GRUB can't save and boot counting silently
    stops (the system still boots, just without the automatic fallback).
  * The counter's rules are unit-tested as a pure-Rust model plus checks on the
    generated grub.cfg (`scripts/src/grub.rs`); menu-only reboots vs. real
    failures were checked headless in QEMU on copies of the disk image.
* Pick a layer by hand with the GRUB entries, or on the kernel command line:
  `aftermath.mode=host`, `aftermath.mode=busybox` (`aftermath.rescue` works too).
* Fallback messages are logged at error level, so they show on screen and in
  `out/vm/serial.log` even with the quiet `loglevel=4`.

### Testing the fallbacks

Test hooks on the kernel command line (`aftermath.test=<name>`):

| Hook | Expect |
|---|---|
| `engine-crash` | engine crashes 3x in full mode → Arch host mode shell |
| `engine-crash-always` | crashes in host mode too → BusyBox rescue shell |
| `strata-fail` | strata fail → engine continues in Arch host mode |
| `no-success` | boot is never marked good (GRUB counts it as failed) |
| `panic` | kernel panic → reboot after 5 s → GRUB boot counter steps down to host, then BusyBox |

```sh
cargo am run --direct --append "aftermath.test=engine-crash"
cargo am run --direct --append "aftermath.test=engine-crash-always"
# Whole chain through GRUB: every boot panics until GRUB picks BusyBox (~2.5 min)
cargo am grub --disk --grubenv aftermath_test=panic
cargo am run --no-build --serial
cargo am grub --disk                 # rebuild the normal disk afterwards
```

`--grubenv KEY=VALUE` writes extra variables into the disk's grubenv;
GRUB passes `aftermath_test` on as `aftermath.test=...`.
`cargo am run` boots the disk with `snapshot=on`, so the boot counter lives
for one QEMU session (it survives reboots inside the VM). Add `--persist`
to keep it across runs.

## Logs

Every `cargo am run` writes the complete serial console (GRUB menu, kernel
log, init messages, the shell) to `out/vm/logs/<date_time>.log`, and
`out/vm/serial.log` always points to the newest one, even when the QEMU
window is used. The kernel prints to both `ttyS0` and `tty0`, and the init
starts a root shell on every console.

The serial log only shows the serial console. To also see what happened in
the QEMU window (tty1), **every console is recorded**: each shell runs under
util-linux `script`, which copies the console output into
`/aftermath/store/logs/<boot time>/<tty>.log` on the strata disk, and a
thread copies the kernel log (all `aftermath-*` messages included) into
`kmsg.log` next to it (the newest 30 boots are kept). The banner on each
console shows where it is logged. Read them on the build host, no root needed
(`debugfs` reads `out/strata.img` directly, even while the VM runs):

```sh
cargo am logs               # newest boot: kmsg.log, tty1.log, ttyS0.log
cargo am logs -f tty1       # only the QEMU window console
cargo am logs --list        # all boots; -b 1 = the boot before, or -b <name>
cargo am logs --raw         # keep colours / escape sequences
```

Without a strata disk the logs go to `/run/aftermath/logs` (RAM, lost at
poweroff). Logs are not size-limited yet; a shell session is only text.

## amctl: Aftermath control (the `systemctl` of Aftermath)

`amctl` is the one front door to the running system: strata, services,
logs, boot choices, the overlay, the keyboard and power. It talks to
aftermath-engine over a Unix socket, `/run/aftermath/control.sock` (one JSON
request per line, see `crates/aftermath-ctl/src/proto.rs`). **Everyone may
ask** (status, lists, logs); **changing things needs root**: the engine checks
the caller's uid with `SO_PEERCRED` on every request.

```sh
amctl status                       # mode, root/overlay, network, strata, services, keymap, uptime, boot id
amctl list                         # installation + integration status, including optional Fedora
amctl enable fedora                # download once, activate without reboot, enable at boot
amctl strata list                  # same list (the original subcommands still work)
amctl strata status ubuntu
amctl strata stop|start|restart ubuntu
amctl strata enable|disable ubuntu # now + at every boot
amctl strata enter ubuntu          # a shell inside Ubuntu, prompt `(ubuntu)root@aftermath`
amctl strata enter ubuntu apt list --installed   # or one command
amctl services list                # see "Services" below
amctl services start|stop|restart|status heartbeat
amctl services enable|disable heartbeat
amctl services reload              # re-read /etc/aftermath/services
amctl logs                         # Aftermath messages of this boot (from kmsg.log)
amctl logs -f --service heartbeat  # follow a service log; --kernel, --console tty1, --list, -n 100
amctl boot next host               # one-shot GRUB choice: normal|host|busybox|clean|serial|debug|none
amctl boot status
amctl overlay status
amctl overlay reset                # wipe the saved system changes at the next boot (--cancel to undo)
amctl keymap                       # configured/loaded layout + a live check (key right of T -> `z`)
amctl keymap set us                # load now and save in /etc/aftermath/aftermath.toml
amctl which tac                    # every copy across strata, which one wins and why
amctl default tac ubuntu           # choose the winner (--reset, list, order ...); see "Strata"
amctl poweroff | amctl reboot
amctl subsystems                   # what the engine offers
amctl --json status                # every command: machine-readable JSON for scripts
amctl completions bash > /usr/share/bash-completion/completions/amctl   # zsh, fish, ... too
```

Output uses the Aftermath colours (red, purple on black); `--no-color` or
`NO_COLOR=1` turns them off, and they are off automatically when piped.

**`strat` stays.** `strat ubuntu apt install foo` and the cross launchers
work as before; `amctl strata enter` is the friendly way to get a shell in a
stratum (it uses `strat` underneath).

**Plan B.** If the engine or its socket is gone (BusyBox rescue, the
engine crashed), amctl says so in red and does what it can by itself:
`status`, `strata list/status`, `services list` (files only) and `logs` are
read straight from /proc, /sys, /run and the log files; as root,
`boot next`, `overlay reset`, `keymap set`, `default` work offline too
(and `which` never needs the engine), and
`poweroff`/`reboot` signal PID 1 or, as a last resort, call the reboot
syscall. amctl is a static binary, so it also runs in the BusyBox rescue
layer.

**Subsystems.** Inside the engine every area is a subsystem behind one
small Rust trait (`crates/aftermath-engine/src/control.rs`: name, read-only
verbs, `call`, an optional status summary, child-exit and timer hooks).
Today: `system`, `strata`, `default`, `services`, `desktop`, `sessions`, `users`, `boot`, `overlay`, `locale`. Future
pieces (a Windows stratum manager, a D-Bus/logind shim for Plasma) plug in
as new subsystems plus an amctl subcommand; nothing else changes.

### Services

The engine supervises services from `/etc/aftermath/services/<name>.toml`
(the file name is the service name). Only `exec` is required:

```toml
description = "Prints a heartbeat every 5 seconds"
exec = ["/usr/bin/bash", "-c", "while true; do echo beat; sleep 5; done"]  # absolute path
stratum = "ubuntu"        # optional: run inside that stratum (via strat); default: the Arch host
user = "tim"              # optional: run as this user (default root)
restart = "always"        # "always" | "on-failure" (default) | "no"
restart_delay = 1         # seconds before a restart (default 1)
after = ["db"]            # start after these (ordering only)
needs = ["db"]            # start these first; stopping db also stops this one
enabled = true            # start at boot (default true)
workdir = "/"             # optional
[env]                     # optional extra environment
FOO = "bar"
```

* Every service runs in its own session/process group; `stop` sends
  SIGTERM to the group and SIGKILL after 5 s.
* stdout and stderr go to `<session log dir>/services/<name>.log`
  (`amctl logs --service <name>`), together with `--- [aftermath ...]` lines
  for every start, exit and restart.
* Restarts: `always` restarts after any exit, `on-failure` after a non-zero
  exit or a signal. **Start limit:** 5 starts within 30 s mark the service
  `failed` instead of looping (`amctl services start <name>` retries).
* Boot: all enabled services start in dependency order after the strata are
  up and the network is online (or the 15 s wait ran out). Broken files are
  skipped and listed by `amctl services list`; cycles are reported.
* `amctl services|strata enable/disable` store their switch in
  `/etc/aftermath/state.toml` (the shipped files keep their comments). That
  file survives reboots with the host overlay disk, not on a pure RAM root.
* `heartbeat.toml` is a shipped example, off by default:
  `amctl services start heartbeat`, then `amctl logs -f --service heartbeat`.

### Default Plasma desktop and device discovery

The default image builds the five local Plasma repositories in `plasma/` and
starts an SDDM graphical login screen. The selected source baseline is Plasma
6.8.3 on local `aftermath` branches; the upstream `master` branches remain
available. Source builds install into `out/plasma/stage`, then into the guest
image, without replacing the build host's desktop. Build logs are in
`out/plasma/logs`. Incremental object builds are retained.

```sh
cargo am plasma                    # build the local Plasma sources and elogind
cargo am reset-vm                  # rebuild/reset, then open the fresh VM
cargo am reset-vm --no-run         # rebuild/reset without opening QEMU
cargo am run                       # rebuild changed inputs and boot normally
```

The reset command still asks before deleting saved VM changes. It compiles
Plasma before deleting the old VM data, so a source build failure leaves saved
data intact. `--keep-rootfs` installs newly required packages before repacking.
The QEMU window's hardware Reset action only reboots the existing image; source
rebuilding happens through the Cargo commands. The default VM uses UEFI/OVMF
and 4 GiB RAM. `--bios` selects legacy firmware; both firmware paths use the
small bootstrap and disk-backed root.

At the graphical screen, use temporary **bob / bob** credentials. The temporary
root password is also **bob**. These are development defaults, initialized once
per fresh host root; changing a password survives ordinary reboots. Set
`[desktop] development_accounts = false` to disable this bootstrap. Set
`[desktop] enabled = false`, or boot with `aftermath.desktop=off`, for consoles only.

Aftermath's Rust engine supervises udev, the system bus, standalone **elogind**
and SDDM. Elogind supplies login1, seats and revocable DRM/input access. The Rust
PAM module registers authenticated sessions while leaving elogind's runtime
directories under elogind ownership. The Rust SDDM session command runs
`dbus-run-session -- startplasma-wayland`; Plasma uses its upstream classic
startup (`systemdBoot=false`). Aftermath remains PID 1. This path does not supply
a general systemd unit interpreter or per-user systemd1 manager.

```sh
# Inside Aftermath (any user; works offline too):
amctl desktop check
amctl --json desktop check
amctl desktop devices           # live udev state (needs the running engine)
amctl desktop bus               # live system D-Bus readiness and supervision
amctl desktop status            # elogind, graphical login and startup errors
amctl sessions list             # authenticated sessions and runtime directories

# On the build host, inspect the guest rootfs, not the host installation:
cargo run -p aftermath-ctl --bin amctl -- desktop check --root out/rootfs
```

The check inventories executable files for a device manager, `udevadm`,
`dbus-daemon`, `dbus-send`, the Rust PAM module, `dbus-run-session`,
`startplasma-wayland`, `kwin_wayland`, elogind, SDDM and the Rust session command in the
**host stratum**. `--root` bypasses the control socket and only inspects the
given tree; absolute symlinks resolve inside that tree. Nothing is installed,
executed, started or enabled. Exit status is **2** when prerequisites remain,
**1** for an invalid root or another command error, **0** for a ready report.
Offline file inventory reports `ready: false` because it cannot check a live
graphical login. The online report also checks the engine's live supervision.
`present` only means an executable file exists; it does not verify package
completeness, daemon readiness, graphics drivers or a working session.

**Device discovery at boot.** The engine starts the installed
`/usr/lib/systemd/systemd-udevd` in the foreground (or a supported `udevd`
location), waits for `udevadm control --ping`, triggers existing subsystems
and devices with `--action=add`, then runs a bounded `udevadm settle`.
This uses the daemon directly; no systemd service manager or unit file is
started. The provider must support these udevadm commands; the Arch provider
in the development rootfs was tested in QEMU. Rules load hardware modules,
populate the device database and apply device names, tags and permissions.
See the [udev manual](https://www.freedesktop.org/software/systemd/man/latest/udev.html)
and [udevadm manual](https://www.freedesktop.org/software/systemd/man/latest/udevadm.html).

Startup has a 25-second overall limit. Daemon crashes retry after one second,
with a limit of five starts in 30 seconds, and repeat discovery on restart.
Subprocess timeouts are enforced by the engine as well as udevadm. Discovery
failures are reported as `degraded`; an already running daemon can continue
handling hotplug. Missing tools or repeated daemon failures also leave boot,
DHCP and the root consoles available. The live command shows `state`, `pid`,
`responding`, `coldplug`, restart count, errors and `/run/aftermath/udev.log`
(RAM, this boot only). `amctl status` also includes device state. Live
`desktop check` includes this state; `--root` only inventories files.
`desktop devices` returns 0 for completed discovery, 2 for other device states,
and 1 when the engine is unavailable or another query error occurs.

Rebuild the guest image with `cargo am run` after changing the engine. Inside
the updated guest, verify:

```sh
amctl desktop devices           # state running, responding yes, coldplug complete
ls -l /dev/dri /dev/input
udevadm info --query=property /dev/dri/card0
cat /run/aftermath/udev.log
```

`aftermath.udev=off` disables this boot step for troubleshooting.
`aftermath.test=udev-fail` tests repeated daemon failure without losing the
console. Neither setting is enabled by default. QEMU validation covered DRM
and input discovery, USB keyboard hotplug/unplug, daemon crash recovery and
the failed-start fallback, with no persistent VM disks attached. Device tags
do **not** grant an ordinary user a login session or managed seat access.

**System D-Bus at boot.** The engine prepares machine identity and `/run/dbus`,
then starts `dbus-daemon` in the foreground using the packaged
`/usr/share/dbus-1/system.conf`. This retains its dedicated service account,
authentication and message policies. `dbus` is now an explicit rootfs package;
older images missing it need `cargo am rootfs --update` before `cargo am run`.
The existing Arch rootfs already contains the binaries and `dbus` account.
`dbus-broker-launch` integration is not implemented in this step.

The bus listens at `/run/dbus/system_bus_socket`. Readiness requires an actual
`org.freedesktop.DBus.ListNames` reply through that address; an existing socket
file alone is insufficient. Replies are checked again every five seconds.
Each probe has a two-second engine-enforced timeout, initial readiness has
a five-second limit, and boot waits at most ten seconds for this subsystem.
Crashes or failed health checks retry after one second, with a limit of five
starts in 30 seconds. Restarting a bus disconnects its existing clients.
Failures leave device discovery, networking and consoles available.

Images ship an empty `/etc/machine-id` and a legacy
`/var/lib/dbus/machine-id` symlink to it. The engine generates an identity
on first boot, before device discovery, and keeps both paths consistent.
The identity persists with the host overlay disk; a RAM-only root gets a new
one on each boot. Valid existing identities are retained, including an older
legacy-only ID. Invalid nonempty or conflicting IDs are reported instead of
silently replaced. The engine refuses to start a second bus on an active
socket, and only removes a stale or previously owned socket.

Inside an updated guest:

```sh
amctl desktop bus               # state running, responding yes
amctl --json desktop bus
dbus-send --system --print-reply --reply-timeout=1000 \
  --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.ListNames
cat /run/aftermath/dbus.log       # RAM log, this boot only
```

`amctl status` and live `desktop check` also show bus state. `desktop bus`
returns 0 for a responding bus, 2 for other bus states, and 1 for query errors
or an unavailable engine. `aftermath.dbus=off` disables this subsystem;
`aftermath.test=dbus-fail` exercises its failed-start fallback. These are off
by default. QEMU checks cover real replies, the service UID, unprivileged
access under the packaged policy, crash recovery, identity persistence across
two boots with a private overlay disk, and continued console/network/device
operation when the bus cannot start.

This provides the **system bus**. Per-user buses, login1
and systemd1 APIs remain future work. The daemon can use its packaged classic
D-Bus activation helper; the engine does not pass `--systemd-activation` or
interpret systemd service units. See the official
[daemon options](https://dbus.freedesktop.org/doc/dbus-daemon.1.html),
[dbus-send options](https://dbus.freedesktop.org/doc/dbus-send.1.html) and
[machine identity specification](https://dbus.freedesktop.org/doc/dbus-specification.html#standard-interfaces-peer).

**Authenticated console sessions.** After rebuilding with `cargo am run`,
Ctrl+Alt+F2 opens a login prompt on tty2. Use an existing account and its
password. The default development image initializes Bob and root as described
above. Additional normal accounts can be created inside the guest from the
development root console:

```sh
amctl users add alice
```

**Local user management.** `amctl users add NAME` creates an ordinary account
with its own group, Bash shell and private 0700 home under `/home`, then opens
an interactive PAM password prompt. New accounts start password-locked; an
interrupted prompt leaves the account locked. Retry with `amctl users passwd NAME`.
There are no preset passwords or automatic administrator permissions.

```sh
amctl users list
amctl users show alice
amctl users passwd alice           # root resets Alice's password
amctl users passwd                 # a logged-in user changes their own password
amctl users add bob --locked       # create without prompting (also supports --json)
amctl users lock alice             # lock password login; active sessions continue
amctl users unlock alice           # restore a previously set, locked password
amctl users sync                   # retry sharing host accounts with running strata
```

Creation, password locking and explicit synchronization require root. Read-only
inventory is available to ordinary users; it reports password status, never hashes.
Password setup requires a terminal and refuses `--json` before changing anything.
Rust code calls the existing `useradd`, `usermod` and PAM-aware `passwd` programs.
Password text stays in the local, echo-disabled prompt; it is never an engine
request or command argument. The development recorder logs terminal output only.

The host owns these accounts. Creation reserves unused regular-user UID/GIDs
across available configured Linux strata and refuses existing names or homes.
Host account changes are merged into running strata with account database locks
and permission-preserving replacement; other strata retain their service accounts.
The engine also checks for changes every three seconds, covering a user's own
password changes and native account tools. Busy/conflicting strata are reported
in engine logs and retried; `amctl users sync` exposes an immediate retry.
Stopped strata receive the host accounts when next started. Create users from
the host console, rather than independently inside each stratum.

Accounts and homes persist with the existing host overlay disk. A RAM-only root
loses them on shutdown. This step does not add account deletion or group editing.

The existing `agetty` and `login` programs use the packaged PAM `system-auth`
policy for password/account checks. The **Rust** `pam_aftermath.so` session
module then asks the engine to register the authenticated session. If that
module or the engine is unavailable, session opening fails; the login terminal
does not fall back to an unauthenticated root shell. Login terminals run
outside the development console recorder, so it does not record password input.

With elogind available, its PAM module owns `/run/user/<uid>` and seat/session
lifetime; the engine validates and registers those directories without deleting
them. In console-only images without elogind, the engine creates its own
`/run/user/<uid>` owned by the user with mode 0700. PAM sets
`XDG_RUNTIME_DIR`, `XDG_SESSION_ID`, `XDG_SESSION_TYPE=tty` and
`XDG_SESSION_CLASS=user`; VT logins also get their VT number and a logical
`seat0` label. This label grants no device control. Concurrent sessions for
one UID share the directory. Final logout detaches it and deletes its contents
in a Rust worker, without following symlinks. Dead PAM leaders are detected
too. Root-owned ownership records allow recovery of Aftermath's stale runtime
directories; unknown directories or replacements are refused rather than deleted.

Use `amctl sessions list` or `amctl --json sessions list` to inspect sessions.
The overview also counts sessions and users. Registration/closure are root-only
PAM requests; the CLI does not expose a user-selectable authentication bypass.
Configure terminals in `/etc/aftermath/aftermath.toml`:

```toml
[console]
login_ttys = ["tty2"]       # [] disables extra login terminals
```

**This remains a development image:** the original root consoles remain on
the boot consoles, normally tty1 and serial. Adding an authenticated terminal
does not make the image suitable for unattended or multi-user deployment.
Graphical sessions have a session-scoped D-Bus and elogind-managed seats,
devices and session cleanup. A general per-user service manager and engine
service cgroups are still missing. The Rust PAM
module links dynamically to existing PAM/libc; PID 1 and the engine stay
static. The build host needs PAM's link library (`pam` on Arch).

The integration test is Rust on both host and guest:

```sh
cargo test --workspace
cargo am smoke-desktop
cargo am smoke-desktop --grub       # include the default UEFI/OVMF → GRUB path
cargo am smoke-desktop --grub --bios
cargo am smoke-sessions             # reuses the private root fixture above
cargo am smoke-live                 # real reboot/persistence/recovery checks, UEFI
cargo am smoke-live --bios
```

The session smoke command boots a private QEMU guest with current binaries,
configuration and no saved VM disks. Its temporary account/helper are never
installed into normal images. It checks account creation, private homes,
interactive setup, a normal user's own password change, password locking/unlocking,
duplicate/invalid-name rejection, secret-free output, wrong-password rejection, normal-user
privileges, PAM environment, concurrent logins, shared runtime lifetime,
unprivileged registration denial, final-logout cleanup and symlink handling.
It also uses a private stratum account namespace to check ID reservation,
database lock contention, synchronization retries and automatic password sharing.
Credentials are not passed as command-line arguments or printed to the console.
This suite isolates Aftermath-owned runtimes by disabling optional elogind in
its private PAM policy; graphical smoke separately covers elogind ownership.
Because unprivileged package extraction strips set-ID modes, its guest probe
restores the signed Arch packages' `passwd` and `unix_chkpwd` modes inside the
disposable guest. Production pacstrap preserves those modes normally.
Artifacts live under `out/vm/session-smoke/`. Supervisor test child programs
are also Rust, compiled once per test process with the host's Rust compiler.

`smoke-desktop` builds and boots a private graphical guest without the
saved VM disks. It checks the temporary Bob/root passwords through PAM, drives
the real SDDM greeter through QMP input, checks Plasma/KWin D-Bus ownership and
Bob's privileges, then verifies logout/runtime cleanup and return to the greeter.
Its logs and screenshots are under `out/vm/desktop-smoke`. `--grub` also tests
the packaged GRUB disk through UEFI/OVMF; `--grub --bios` selects legacy firmware.
Both use a roughly 2 MiB bootstrap, a disk-backed SquashFS root and 4 GiB RAM.

`smoke-live` reuses that immutable fixture with new, disposable boot/overlay
disks. In one QEMU process it tests actual reboots: saved `/root` and `/etc`
files, lower-file whiteouts, clean-image isolation and restoration, successful
GRUB acknowledgement, an unconfirmed launch counted once, four-failure rescue,
and a normal entry with its root image removed. Rescue reads saved data while
using only the bootstrap. These recovery tests run with 2 GiB RAM; artifacts
are under `out/vm/live-smoke-{uefi,bios}`. The user's saved disks are not attached.

The report lists compatibility limits separately from installed files:

1. **Device seat access**: udev discovery and standalone elogind integration
   supply session access. Test actual hardware and VT switching separately.
2. **Per-user bus**: the default desktop has a private session bus. There is
   no shared per-user service manager across unrelated login sessions.
3. **Seats**: elogind supplies `org.freedesktop.login1`; the engine supervises
   it and registers its authenticated PAM sessions.
4. **Service lifecycle**: service cgroups, readiness notification and activation.
5. **User sessions**: systemd unit interpretation and a per-user `systemd1`
   manager for the desktop's unit jobs, environment and graphical-session targets.
6. **Console handoff**: SDDM hands a VT to the desktop user. The graphical smoke
   test checks login, logout and return to the greeter; fallback consoles remain.

These distinguish the implemented desktop path from remaining compatibility
work. Each desktop startup path has its own requirements.
Plasma's exact requirements depend on its installed version and build; its
[startup source](https://github.com/KDE/plasma-workspace/blob/v6.8.3/startkde/startplasma.cpp)
uses session D-Bus and has systemd integration. The
[login1 API](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)
describes seat/session and device-control behavior that a replacement must
provide. Aftermath integrates [elogind](https://github.com/elogind/elogind)
as its standalone login1 provider.

The private QEMU test covers rendered Plasma Wayland, input through SDDM,
session permissions, D-Bus, login/logout and shutdown. Actual GPU hardware and
VT switching need separate validation. Further integration includes portals,
audio and network/power services as appropriate. The default desktop runs
from the Arch host stratum; installing another desktop into Ubuntu/Fedora
does not give it a separate seat or session manager.

## Keyboard layout (German by default)

One place: `/etc/aftermath/aftermath.toml` (`config/overlay/etc/aftermath/aftermath.toml`):

```toml
[locale]
keymap = "de-latin1-nodeadkeys"   # Linux console
xkb_layout = "de"                 # X11 / Wayland (future desktop)
xkb_variant = "nodeadkeys"
xkb_model = "pc105"
xkb_options = ""
```

At boot, before any shell starts, the engine:

1. writes `/etc/vconsole.conf` (`KEYMAP=...`) and
   `/etc/X11/xorg.conf.d/00-keyboard.conf` (`XkbLayout "de"` ...) from it,
   so a future X11/Wayland desktop gets the same layout;
2. loads the console keymap with kbd's `loadkeys` (`kbd` is in
   `config/packages.x86_64`). **Plan B:** if `loadkeys` is missing or fails,
   a built-in Rust loader feeds a precompiled keymap
   (`/usr/share/aftermath/keymaps/<name>.bmap`, made by `cargo am initramfs`
   with the build host's `loadkeys -b`) to the kernel via the `KDSKBENT`
   ioctl. If both fail the kernel's US layout stays, and the engine logs it.

Check it with `amctl keymap` (it also reads the live keymap back: the key to
the right of T must type `z`) or `dumpkeys | grep "^keycode  21"`. Change it
with `amctl keymap set <name>` (loads it now and saves it) or by editing the
file. `kmsg.log` shows `console keymap de-latin1-nodeadkeys loaded (loadkeys)`.

* **QEMU** passes raw scancodes from your keyboard to the guest, so no
  `-k de` option is needed: the guest's keymap decides, as on real hardware.
* **GRUB** stays on the US layout (not done, on purpose). GRUB keymaps
  (`grub-kbdcomp`, which needs `ckbcomp` from Debian's console-setup) only
  apply to GRUB's own `at_keyboard`/`usb_keyboard` input drivers, not to
  the firmware console that GRUB uses under UEFI and BIOS. Switching GRUB to
  those drivers is fragile on real hardware (it can lose the keyboard
  completely). The menu only needs arrows, Enter and `e`/`c`, which are the
  same on every layout. Only when editing a boot entry are y/z and symbols
  in the US positions.

## Repository layout

```
aftermath_os/
├── Cargo.toml              Cargo workspace (scripts + crates/*; ./linux excluded)
├── .cargo/config.toml      defines the `cargo am` / `cargo xtask` aliases
├── scripts/                the build tool ("xtask"), written in Rust
│   └── src/{main,ctx,deps,kernel,image,qemu,stratum,logs,persist}.rs
├── crates/
│   ├── aftermath-init/     PID 1: minimal supervisor + fallbacks; aftermath-early (persistent root)
│   ├── aftermath-engine/   services, strata, control socket, keymap, boot success (allowed to fail)
│   ├── aftermath-strata/   strata setup service + `strat` multicall launcher
│   ├── aftermath-ctl/      `amctl` + the shared control protocol, facts, service format, keymaps
│   └── aftermath-pam/      Rust PAM session module (shared library; existing PAM authentication)
├── config/
│   ├── grub/themes/aftermath/theme.txt   GRUB theme (images + fonts are generated)
│   ├── packages.x86_64     packages pacstrap installs into the rootfs
│   ├── pacman.conf         plain Arch repos used for pacstrap
│   ├── kernel/aftermath.config   Kconfig fragment Aftermath needs
│   └── overlay/            files copied on top of the rootfs
│       ├── etc/hostname, usr/lib/os-release
│       ├── etc/aftermath/strata.toml        (strata + cross precedence config)
│       ├── etc/aftermath/aftermath.toml     (system settings: [locale] keyboard)
│       ├── etc/aftermath/services/*.toml    (supervised services; heartbeat example)
│       └── aftermath/strata/{arch,ubuntu,windows}/ (stratum mount points)
├── linux/                  Linux kernel source (its own git repo, NOT in this repo; see below)
└── out/                    ALL build outputs (gitignored, created by the tool)
```

## Prerequisites

An Arch-based host (Arch, EndeavourOS, ...) with Rust installed (`rustup`).
The tool tells you exactly what is missing:

```sh
cargo am check-deps
```

It prints a table and a ready-to-paste `sudo pacman -S --needed ...` line.
The packages involved are: `bc base-devel flex bison perl libelf openssl
pahole` (kernel), `arch-install-scripts archlinux-keyring` (rootfs),
`libarchive zstd squashfs-tools` (bootstrap and live root), `grub libisoburn mtools ttf-dejavu` (GRUB disk, theme, ISO),
`qemu-desktop edk2-ovmf` (QEMU), `debootstrap ubuntu-keyring e2fsprogs`
(Ubuntu stratum, optional), `kbd` (precompiled console keymaps, optional).
For fast VMs your user needs access to `/dev/kvm`.

### Kernel source

`./linux` is the Linux kernel source, its own git repository, and is **not**
part of this repo (`/linux/` is in `.gitignore`; it is huge). Get it once
next to the other files. Aftermath currently builds the tree at commit
`63fdb511c27a` (7.4-rc3+, "Merge tag 'mm-core-7.4-rc4'"), cloned from Arch's
kernel mirror:

```sh
git clone https://github.com/archlinux/linux.git linux
git -C linux checkout 63fdb511c27a
cargo am kernel          # builds out of tree into out/, ./linux stays clean
```

Any recent mainline tree works as well; `config/kernel/aftermath.config`
adds the options Aftermath needs (Landlock, overlayfs, virtio, ...).

## How to run the tooling

### What is "xtask"?

Instead of a pile of shell scripts, Aftermath's build steps live in a small
Rust program in `scripts/`. This is a common Rust pattern called **xtask**:
the build tooling is just another crate in the workspace, so it is
type-checked, cross-platform-ish, and you need nothing but `cargo` to run it.

You never have to run it by path. `.cargo/config.toml` defines a **cargo
alias**:

```toml
[alias]
am    = "run --quiet --package xtask --"
xtask = "run --quiet --package xtask --"
```

So `cargo am iso` really means "compile the `xtask` crate if needed, then run
it with the argument `iso`". `cargo xtask iso` is the same thing. It works
from anywhere inside the repo. The first run takes a few seconds to compile
the tool, after that it starts instantly.

Every command has help: `cargo am --help`, `cargo am run --help`, ...

### Root (sudo) and dry runs

Some steps must create root-owned files (the rootfs). Those steps call `sudo`
themselves and print each root command in yellow as `==> [sudo] ...` before
running it, so sudo will ask for your password at that moment. Nothing else
runs as root, and you never run `sudo cargo ...`.

Add `-n` / `--dry-run` to any command to see every command it *would* run
without running anything:

```sh
cargo am --dry-run iso
```

### The commands

| Command | What it does | Root? |
|---|---|---|
| `cargo am check-deps` | Lists host tools, prints the pacman command for missing ones | no |
| `cargo am kernel` | Builds `./linux` out-of-tree into `out/kernel-build`, copies the kernel to `out/kernel/` and installs modules to `out/kernel/modules/` | no |
| `cargo am init` | Builds `aftermath-init` (PID 1) and `aftermath-engine` as static binaries | no |
| `cargo am rootfs` | `pacstrap`s a fresh Arch rootfs into `out/rootfs` | **yes** |
| `cargo am grub [--disk]` | Generates the GRUB menu, theme images (Rust-drawn PNGs) and fonts in `out/grub`; `--disk` also builds the BIOS+UEFI boot disk image | no |
| `cargo am initramfs` | Stages init/config/modules, builds `out/rootfs-aftermath.squashfs` and a small `out/initramfs-aftermath.img` | **yes** |
| `cargo am iso` | Builds whatever is missing, then a BIOS+UEFI bootable ISO | yes if rootfs/initramfs need (re)building |
| `cargo am run` (alias `qemu`) | Builds whatever is missing or outdated, then boots QEMU through GRUB (or `--direct` / `--iso`) | same as above |
| `cargo am stratum ubuntu\|image\|list\|remove` | Build / list / remove strata and the strata disk `out/strata.img` (see *Strata*) | **yes** (except `list`) |
| `cargo am reset-vm [--yes] [--keep-strata] [--keep-rootfs] [--logs]` | Wipes everything the VM changed and rebuilds a fresh VM (asks first) | **yes** |
| `cargo am logs [-f tty1] [-b N] [--list]` | Prints the VM's session logs (every console + kernel log) from the strata disk | no |
| `cargo am smoke-desktop [--grub] [--bios]` | Builds a private live image, verifies rendered Plasma Wayland login/logout | no |
| `cargo am smoke-sessions` | Rust host/guest user/PAM tests on the private SquashFS root | no (run graphical smoke first) |
| `cargo am smoke-live [--bios]` | Real reboot tests for saved changes, clean image, GRUB counter and independent rescue | no (run graphical smoke first) |
| `cargo am clean` | Deletes generated files in `out/` | only with `--rootfs`/`--strata`/`--all` |

Examples:

```sh
# Kernel (first build takes a while; later builds are incremental)
cargo am kernel
cargo am kernel --menuconfig        # tweak the config, then build
cargo am kernel --reconfigure       # regenerate .config from the fragments
cargo am kernel -j 8

# Rootfs (needs network; downloads Arch packages)
cargo am rootfs
cargo am rootfs --force             # throw away and recreate
cargo am rootfs --update            # install packages newly added to config/packages.x86_64
cargo am rootfs -p vim -p htop      # extra packages this time
#   permanent extras: add them to config/packages.x86_64, then --force

# Boot in QEMU through GRUB (the themed "Aftermath OS" menu)
cargo am run                        # window (GTK), UEFI/OVMF, 4G RAM, 4 CPUs, KVM
cargo am run --uefi                 # explicit UEFI (already the default)
cargo am run --bios                 # legacy firmware, same SquashFS root
cargo am run --direct               # skip GRUB: QEMU loads kernel + initramfs (fastest)
cargo am run --direct --debug       # + earlyprintk, ignore_loglevel, loglevel=8
cargo am run --headless             # no window, only the serial log (for test boots)
cargo am run --persist              # keep grubenv (boot counter) across runs
cargo am run --serial               # no window: everything in this terminal
                                    #   quit QEMU with Ctrl-A then X
cargo am run -m 8G -c 8             # more RAM / CPUs
cargo am run --disk 20G             # attach persistent out/vm/disk.qcow2
cargo am run --direct --append "aftermath.rescue"   # extra kernel parameters
cargo am run --no-build             # boot exactly what is in out/
cargo am run -- -s -S               # anything after -- goes to QEMU (here: wait for gdb)

# ISO
cargo am iso                        # -> out/aftermath-0.1.0-x86_64.iso
cargo am run --iso                  # boot the ISO like real hardware
cargo am run --iso --bios           # ... with legacy firmware

# Cleaning
cargo am clean                      # initramfs, ISO, staging (keeps kernel + rootfs)
cargo am clean --kernel             # also the kernel build
cargo am clean --vm                 # also UEFI vars + VM disk
cargo am clean --rootfs             # also the rootfs (sudo rm -rf out/rootfs)
cargo am clean --all                # everything in out/
cargo clean                         # Rust build output (target/)
```

**Typical first session:**

```sh
cargo am check-deps        # install what it lists
cargo am kernel
cargo am rootfs            # sudo
cargo am run               # sudo once (packs the initramfs), then the GRUB menu opens
```

**Day-to-day loop when hacking on the init:** edit
`crates/aftermath-init/src/main.rs`, then `cargo am run`. The tool notices
that init changed, rebuilds it, repacks the initramfs (sudo) and boots.

Inside the VM you get a root shell. `poweroff` / `reboot` are handled by
aftermath-init itself.

### Where outputs land

Everything goes into `out/` (gitignored); Rust binaries go into `target/`.

| Path | What |
|---|---|
| `out/kernel-build/` | kernel object files + `.config` |
| `out/kernel/vmlinuz-aftermath` | the kernel image |
| `out/kernel/modules/` | kernel modules (`usr/lib/modules/<version>`) |
| `out/rootfs/` | the Arch rootfs (**root-owned**, remove with `cargo am clean --rootfs`) |
| `out/overlay/` | staging copy of what gets added to the rootfs |
| `out/initramfs-aftermath.img` | small static bootstrap and independent BusyBox rescue, zstd cpio |
| `out/rootfs-aftermath.squashfs` | compressed read-only root, mounted from disk/ISO on demand |
| `out/grub/` | generated GRUB tree: `boot/grub/{grub.cfg,fonts,themes}` |
| `out/aftermath-<version>-x86_64.img` | GRUB boot disk (BIOS + UEFI), used by `cargo am run` |
| `out/iso-root/` | ISO contents before packing |
| `out/aftermath-<version>-x86_64.iso` | the bootable ISO |
| `out/vm/logs/<date_time>.log` | full serial console of every `cargo am run` (GRUB menu, kernel log, shell) |
| `out/vm/serial.log` | symlink to the newest log |
| `out/host-overlay.img` | host overlay disk (ext4, label AFTERMATH_HOST): your saved changes, attached by `cargo am run` |
| `out/strata/<name>/` | built strata (**root-owned**) |
| `out/strata.img` | the strata disk (ext4, label AFTERMATH_STRATA), attached by `cargo am run` |
| `out/vm/` | other QEMU state: `OVMF_VARS.4m.fd`, `disk.qcow2` |
| `target/x86_64-unknown-linux-gnu/release/aftermath-init` | the init binary |

### Writing the ISO to a USB stick

```sh
sudo dd if=out/aftermath-0.1.0-x86_64.iso of=/dev/sdX bs=4M status=progress oflag=sync
```

(Double-check `/dev/sdX`; this wipes the stick.)

## Next steps

- **Init / desktops**: services exist now (TOML, needs/after, restart, logs,
  `amctl`), and `amctl desktop check` reports installation and engine gaps.
  The default source-built Plasma has graphical login, a session bus, Rust PAM
  registration and elogind seat/device control. Next, one step at a time:
  service cgroups/readiness and the general user-session/systemd compatibility
  layer. See *Default Plasma desktop and device discovery* above.
- **Boot layout**: disk-backed SquashFS is now the overlay's lower layer.
  Next: an installer and validation on physical hardware.
- **Strata**: more downloadable strata (e.g. Alpine), cross
  `.desktop` Exec rewriting, a FUSE crossfs instead of generated symlinks.
- **Windows stratum**: research phase.

## License

Aftermath's own code: GPL-2.0-or-later (to be confirmed). The kernel in
`./linux` is GPL-2.0.
