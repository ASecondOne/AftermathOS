# Aftermath

> **Status: experimental.** Aftermath boots to a root shell in QEMU at best.
> Expect breakage, don't install it on hardware you care about.

Aftermath is an Arch-based Linux distribution with **no systemd**. Its PID 1 is
a custom init written in Rust, and it ships **Bedrock Linux-style strata**:
several complete userlands living side by side, managed by a separate strata
service that runs next to the normal Arch boot.

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
             └─ initramfs = the whole Arch rootfs (zstd cpio, unpacked into RAM)
                  └─ rdinit=/usr/bin/init → aftermath-init (PID 1, tiny, never panics)
                       ├─ aftermath-early: image + overlay disk → persistent / (fail → RAM root)
                       ├─ mounts /proc /sys /dev /run /tmp, cgroup2; sets hostname
                       ├─ reaps zombies; poweroff/reboot/halt on signal
                       └─ supervises /usr/lib/aftermath/aftermath-engine
                            ├─ network: lo + dhcpcd
                            ├─ aftermath-strata setup: strata disk → /aftermath/strata/ubuntu,
                            │    shared mounts, cross launchers  ── fail → plain Arch host mode
                            ├─ root shell on every console (ttyS0 + tty1)
                            └─ marks the boot good in GRUB's grubenv
                       (engine keeps crashing → host mode → BusyBox rescue, see "Fallback layers")
```

The rootfs is built with `pacstrap` from `config/packages.x86_64`. That list
deliberately avoids Arch's `base` meta package, because `base` pulls in
systemd. (Some packages still need `systemd-libs`, which is only a library.)

Shipping the rootfs as one big initramfs keeps the first prototype simple: no
mkinitcpio, no squashfs, no `switch_root`. A real live/installed layout comes
later (see *Next steps*).

## Strata: Arch + Ubuntu side by side (Bedrock style)

Aftermath ships two strata: **arch** (the host, i.e. the booted rootfs `/`)
and **ubuntu** (Ubuntu 24.04 LTS "noble", in `/aftermath/strata/ubuntu`).
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

* **Storage: a separate strata disk.** The initramfs is unpacked into RAM, so
  putting Ubuntu (~0.5-1 GB) there would cost that much RAM on every boot and
  everything `apt` installs would vanish on reboot. A squashfs would fix the
  RAM cost but is read-only (apt then needs an overlay whose upper layer again
  needs a disk). So strata live on their own **ext4 disk** (`out/strata.img`,
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
  "initialise" each interface; there is no udevd, so `eth0` stayed DOWN.
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
| `cargo am run --no-overlay` / `--overlay-snapshot` | boot without the host overlay disk (RAM root) / throw away new changes | no |
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

The image (the initramfs) stays a clean, read-only base. A small extra disk,
`out/host-overlay.img` (2G sparse, ext4 label `AFTERMATH_HOST`, created by
`cargo am run` without root), holds everything you change in the VM:

```
 aftermath-init (PID 1) ──exec──▶ aftermath-early (same PID, before anything else)
     finds the AFTERMATH_HOST disk, mounts it, then
     overlayfs:  lower = the image (RAM, read-only)   upper = <disk>/upper
     moves the overlay onto / (switch_root style) ──exec──▶ aftermath-init again
     anything wrong ──▶ exec aftermath-init on the plain RAM root (plan B)
```

* `pacman -Syu`, installed packages, `/root`, `/etc` edits survive reboots.
  The console banner says which one you got: *Changes to the system are saved*
  or *RAM root: changes are lost at poweroff*.
* The disk stays mounted at `/aftermath/hostdisk` (`upper/` = your changes).
* Paths the image owns are removed from the saved changes at every boot
  (`/usr/lib/aftermath`, `/usr/bin/aftermath-init`, `/usr/lib/modules`,
  `/boot`, `/etc/aftermath/{version,build-id}`), so a rebuilt image always
  brings its own init, engine and strata service.
* Every image has a build id; when the image was rebuilt under existing saved
  changes, aftermath-early logs a warning (packages you upgraded in the VM
  shadow the image's newer ones; if that bites: "clean image" or `reset-vm`).
* **Plan B**: no disk → RAM root (normal). Mount error, or saved changes that
  hide `/usr/bin/init`, the engine or bash → logged as `persistent root FAILED`
  and the boot continues on the RAM root. The GRUB entry **"clean image, ignore
  saved changes"** (`aftermath.overlay=off`) and the BusyBox rescue layer never
  use the overlay; in rescue the disk is mounted read-only under `/mnt/<dev>` so
  you can repair `upper/`.
* RAM: the image still lives in RAM (it is the lower layer); only your changes
  go to disk. A squashfs image to cut RAM use is a later step.

```sh
cargo am run --no-overlay          # RAM root, nothing saved
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
  the kernel and the initramfs.
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
| before layer 1 | aftermath-early sets up the persistent root (host overlay disk) | missing/broken disk or saved changes that hide init → plain RAM root |
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
  ISO): every boot GRUB sets `boot_success=0` in `/boot/grub/grubenv`. Once
  the engine is up it sets `boot_success=1 boot_fails=0` (it finds the FAT
  boot partition itself and rewrites the 1024-byte block in place, like GRUB
  does). If a boot never gets that far:
  * 1 failed boot → try **Aftermath OS** again
  * 2–3 failed boots → default becomes **Arch host only**
  * 4+ failed boots → default becomes **rescue: BusyBox CLI**
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
amctl strata list                  # arch (host), ubuntu, windows (+ state, on at boot?, OS name)
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
Today: `system`, `strata`, `default`, `services`, `boot`, `overlay`, `locale`. Future
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
│   └── aftermath-ctl/      `amctl` + the shared control protocol, facts, service format, keymaps
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
`libarchive zstd` (initramfs), `grub libisoburn mtools ttf-dejavu` (GRUB disk, theme, ISO),
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
| `cargo am initramfs` | Copies init + `config/overlay` + kernel modules into the rootfs and packs it as `out/initramfs-aftermath.img` | **yes** |
| `cargo am iso` | Builds whatever is missing, then a BIOS+UEFI bootable ISO | yes if rootfs/initramfs need (re)building |
| `cargo am run` (alias `qemu`) | Builds whatever is missing or outdated, then boots QEMU through GRUB (or `--direct` / `--iso`) | same as above |
| `cargo am stratum ubuntu\|image\|list\|remove` | Build / list / remove strata and the strata disk `out/strata.img` (see *Strata*) | **yes** (except `list`) |
| `cargo am reset-vm [--yes] [--keep-strata] [--keep-rootfs] [--logs]` | Wipes everything the VM changed and rebuilds a fresh VM (asks first) | **yes** |
| `cargo am logs [-f tty1] [-b N] [--list]` | Prints the VM's session logs (every console + kernel log) from the strata disk | no |
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
cargo am run                        # window (GTK), BIOS, 4G RAM, 4 CPUs, KVM
cargo am run --uefi                 # same with UEFI (adds "UEFI Firmware Settings")
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
cargo am run --iso --uefi           # ... with UEFI

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
| `out/initramfs-aftermath.img` | rootfs packed as zstd cpio |
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

- **Init**: services exist now (TOML, needs/after, restart, logs, `amctl`);
  next: cgroup per service, readiness notification, proper getty/login
  instead of the root shell, udev replacement or eudev, then a D-Bus layer
  implementing logind/systemd1 for Plasma (as an amctl subsystem).
- **Boot layout**: squashfs image as the overlay's lower layer (today the
  lower layer is the RAM initramfs), so RAM use no longer equals the rootfs
  size; then an installer.
- **Strata**: more strata (Fedora via dnf --installroot, Alpine), cross
  `.desktop` Exec rewriting, a FUSE crossfs instead of generated symlinks.
- **Windows stratum**: research phase.

## License

Aftermath's own code: GPL-2.0-or-later (to be confirmed). The kernel in
`./linux` is GPL-2.0.
