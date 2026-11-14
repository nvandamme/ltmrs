# Power rig runbook

`cargo xtask power` replaces the tribal-VM knowledge for durability trials
with pure-Rust verbs (zero shell scripts). Guest-side snippets are inline
strings passed over SSH — data, never control flow. Every hypervisor spawn
goes through one cfg-gated site (`spawn_control` in `xtask/src/power.rs`);
on non-Linux it compiles to a stub reporting
"unsupported platform — Hyper-V CLI backend not yet ported".

## Prerequisites

- Linux host with libvirt (`virsh`), `qemu-img`, and an OpenSSH client.
- Default libvirt NAT network (`default`) with DHCP; the golden image uses
  static `192.168.122.10` on `eth0`.
- Default storage pool (`default`) holding the golden volume
  `ltmrs-golden-vol`.
- Host SSH keypair with the public half staged at `/root/key.pub` inside
  the build environment; the build installs it as
  `/root/.ssh/authorized_keys` (mode 600) on the image.
- Base kernel/initrd at `/var/lib/libvirt/images/vmlinuz-virt` and
  `initramfs-virt` (see `tools/vm/domain-template.xml`: 2 vCPU, 2048 MiB,
  virtio disk on `vda`, virtio NIC on `default`, deterministic MAC from the
  trial name under QEMU OUI `52:54:00`).

## Build → trial → evidence

```
cargo xtask power --op build [--force] [--dry-run]
cargo xtask power --op trial --mode canary|daemon --name <trial> [--dry-run]
cargo xtask power --op clean --name <trial> [--dry-run]
cargo xtask power --op clean --stale [--dry-run]
```

- `build` creates the 8G qcow2 golden volume, defines/boots
  `ltmrs-golden`, provisions it over SSH (mkfs, tree copy, static net,
  autologin, device nodes, `authorized_keys`, `S99power` runlevel link),
  then destroys/undefines it and asserts `vol-info`. Without `--force` it
  refuses when the golden volume already exists. The name `ltmrs-golden`
  is reserved: no trial or clean may use it (golden guard).
- `trial` clones the golden volume (`vol-clone`), defines/boots
  `power-<name>`, waits up to 60s for SSH (`BatchMode=yes`,
  `ConnectTimeout=2`, 2s between probes), runs the mode workload
  (`tools/vm/<mode>-workload.txt`, one `ssh` spawn per line), cuts power
  (`virsh destroy` — never graceful shutdown), recreates the domain from
- `clean` destroys, undefines, and vol-deletes `power-<name>`.
- `clean --stale` sweeps every leftover `power-*` domain/volume (failed trials keep theirs for forensics) — destroy (best-effort, absent domains treated as already clean), undefine, vol-delete — and writes `reports/power-clean-stale/record.json`. Only names starting with `power-` are touched, so the golden and host domains are never matched.
- `--dry-run` prints every would-be argv (generated from the same step
  commands the live body runs, so the preview cannot drift) and spawns
  nothing.
- Evidence lands under `reports/power-build` (build) and
  `reports/power-<name>` (trial): build/clean write `record.json` (argv,
  exit code, tails, verdict + steps in `meta`); trials write `verdict.json`
  (verdict + steps inline) plus the stamped `<guest>.xml`.

## Verdict schema (`verdict.json`)

```json
{
  "mode": "canary|daemon",
  "name": "<trial>",
  "verdict": "pass|fail|inconclusive",
  "steps": [{"name": "<step>", "exit_code": 0, "stderr_tail": "<tail>"}],
  "kept_volume": false
}
```

- `pass`: every step exited 0.
- `fail`: a step (workload, verify, …) exited non-zero — the guest or the
  pipeline misbehaved.
- `inconclusive`: a harness fault (spawn error, SSH unreachable after 60s,
  XML/record write failure) — the rig, not the code under test, is
  suspect. Precedence is `inconclusive` > `fail` > `pass`: a later step
  never downgrades an earlier verdict.
- `kept_volume` is exactly `verdict != "pass"`: an advisory flag in
  `verdict.json` — nothing consumes it. Trial volumes accumulate under the
  `default` pool until `clean --name <trial>` destroys/undefines/vol-deletes
  them, pass or not.
- Op exit codes: 0 on `pass`, 1 otherwise. `127` never appears as a process
  exit — it marks harness-fault steps inside `record.json`/`verdict.json`
  step outcomes, so never branch on `$? == 127`.

## Durability rules

- Fresh clone per trial: every trial starts from `vol-clone` of the
  golden image, never from a reused or snapshotted volume.
- Power cut, not shutdown: `virsh destroy` between workload and verify,
  so the verify step observes post-crash state, not a cleanly flushed one.
- Recreate before verify: the domain boots again from the same XML, then
  `verify` reads the mode marker (`/var/lib/power-canary`, or the
  daemon's `receipts.json`) plus the journal.
- Daemon-side invariant (tested, no hypervisor needed):
  `recovery_reopen_keeps_receipts` and
  `recovery_reopen_has_no_partial_merge` in
  `crates/daemon/src/server/lifecycle_tests.rs` commit memories and a
  merge, drop the store handle without closing (simulated kill -9),
  reopen, and assert every receipt/memory survives and no relation names
  a missing endpoint. Receipts commit atomically with their effects in one
  Fjall transaction; unknown commit outcomes resolve via re-reading the
  receipt under the same operation key.

## debugfs discipline

Live-volume forensics (when a kept volume needs inspection) follow one
batched session: mount or `debugfs` once, run every read command in that
single session, then close. Never interleave writes between reads.
`e2fsck -n` (read-only) captures come first, before any targeted read,
so later inspection cannot be blamed for changing what it observes.

## Failure playbook

- `fail` on `workload`/`verify`: read the step's `stderr_tail` in
  `verdict.json`, then triage the kept volume (`virsh vol-info`,
  guest-side marker, `dmesg` tail). Fix the code or the workload, `clean`
  the trial, re-run with a fresh clone.
- `inconclusive`: suspect the rig first — libvirt running? `default`
  network/pool present? golden volume intact? guest reachable by SSH?
  Re-run; two inconclusives in a row mean the rig is broken, not the
  trial.
- Kept-volume triage: trial volumes persist regardless of verdict (see
  above). Inspect via the debugfs discipline above, then `clean --name
  <trial>` to destroy/undefine/vol-delete.
- Stale `power-<name>` leftovers from an interrupted run: `clean --stale` sweeps them all (destroy best-effort, then undefine + vol-delete per guest, record at `reports/power-clean-stale/record.json`). Single-trial cleanup stays `clean --name <trial>`.

## Host-mount discipline

Host-side mounts (loop devices, ISO images, qcow2 assembly in the future
build op) live ONLY under repo-owned `tmp/vm/mnt-*`, created per use and
removed after unmount in the same op. NEVER `/`, `/mnt`, or any bare
system path — the manual sessions leaked an ISO mount on `/mnt` and loop
devices on deleted images, requiring superuser cleanup. `virsh`/`qemu-img`
never mount anything; only a future build step that explicitly loops a
host file must follow this rule.
- Windows: every `power` verb fails fast with the Hyper-V stub message;
  run the rig on Linux.
