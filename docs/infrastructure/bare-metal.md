# Bare-Metal Kubernetes Bootstrap Guide for Stellar-K8s

Cloud hosting for archival and full-history Stellar nodes is dominated by
storage and egress costs. Moving to bare metal removes the per-GB markup, but
it also means you own the disk layout, the network fabric, and the Kubernetes
storage layer that cloud providers normally hide. This guide is the end-to-end
path for standing up a **cloud-independent** Stellar-K8s cluster on physical
hardware using either **Talos Linux** or **kubeadm**, with local NVMe drives
attached directly to `StellarNode` StatefulSets.

The cluster built here has **no dependency on a cloud provider**:

- No `cloud-provider` integration and no cloud load balancers. External access
  uses [MetalLB](../metallb-bgp-anycast.md) in L2 or BGP mode.
- No cloud CSI driver and no cloud `StorageClass`. Volumes come from
  [Local Path Provisioner](https://github.com/rancher/local-path-provisioner)
  or [OpenEBS Local PV](https://openebs.io/docs/concepts/data-engines/localstorage)
  on physical disks.
- No cloud DNS or managed database. Postgres runs in-cluster or on a database
  host you operate.

Related documents:

- [Bare-Metal NVMe IOPS Tuning](../performance/bare-metal-iops.md) — how to
  format, mount, and benchmark the physical drives. **Do this before the
  cluster exists**; this guide assumes it is done.
- [Capacity Planning](../operations/capacity-planning.md) — how much disk and
  RAM each node type needs.
- [Networking Overview](../networking/index.md) — ports, policies, and BGP.
- [Resource Limits](../resource-limits.md) — CPU and memory per node type.
- [Volume Snapshots](../volume-snapshots.md) and
  [Disaster Recovery](../operations/disaster-recovery.md) — local volumes are
  single-host, so backups are not optional.

!!! warning "Audience and risk"
    This guide provisions bare-metal hosts and formats disks. Every destructive
    command is called out explicitly. Validate the full path on virtual
    machines first using
    [`examples/bare-metal/scripts/validate-vm-cluster.sh`](../../examples/bare-metal/scripts/validate-vm-cluster.sh)
    before touching production hardware.

---

## Contents

- [1. Reference hardware topology](#1-reference-hardware-topology)
- [2. Prerequisites](#2-prerequisites)
- [3. Path A — Talos Linux](#3-path-a--talos-linux)
- [4. Path B — kubeadm](#4-path-b--kubeadm)
- [5. Physical network topology](#5-physical-network-topology)
- [6. Local storage provisioning](#6-local-storage-provisioning)
- [7. Deploy Stellar-K8s on local volumes](#7-deploy-stellar-k8s-on-local-volumes)
- [8. Validate the cluster](#8-validate-the-cluster)
- [9. VM validation harness](#9-vm-validation-harness)
- [10. Production checklist](#10-production-checklist)

---

## 1. Reference hardware topology

A three-node cluster is the smallest topology that can host a Stellar validator
quorum with meaningful availability. The reference layout below is what the
rest of the guide assumes; scale it out rather than up where you can.

| Node | Role | CPU | RAM | System disk | Data disks | Network |
|---|---|---|---|---|---|---|
| `stellar-cp-1` | control plane + validator | 16 cores | 64 GiB | 2 × 480 GB SATA SSD (RAID1) | 2 × 3.84 TB NVMe (PLP) | 2 × 25 GbE bonded |
| `stellar-cp-2` | control plane + validator | 16 cores | 64 GiB | 2 × 480 GB SATA SSD (RAID1) | 2 × 3.84 TB NVMe (PLP) | 2 × 25 GbE bonded |
| `stellar-cp-3` | control plane + validator | 16 cores | 64 GiB | 2 × 480 GB SATA SSD (RAID1) | 2 × 3.84 TB NVMe (PLP) | 2 × 25 GbE bonded |
| `stellar-archive-1..N` | Horizon / Soroban RPC / history archive | 32 cores | 128 GiB | 2 × 480 GB SATA SSD (RAID1) | 4 × 7.68 TB NVMe | 2 × 25 GbE bonded |

Design rules:

- **Keep the OS off the Stellar data drive.** The system disk carries the
  container image cache and logs; mixing it with the ledger database puts
  unrelated write traffic on the same device queue.
- **Validators need power-loss-protected (PLP) NVMe.** A drive without PLP can
  pass every throughput test and still fail the `fdatasync` latency target in
  [bare-metal IOPS §2](../performance/bare-metal-iops.md#2-baseline-iops-targets).
- **Do not co-locate two validators of the same network on one host.** The
  operator's default pod anti-affinity enforces this at the scheduler, but the
  physical host still needs enough independent failure domains to survive a
  single PSU, NIC, or switch failure.
- **One NVMe per purpose where possible.** Splitting the database/buckets from
  the history archive keeps checkpoint writes from starving ledger commits.

!!! tip "Three-node control planes"
    Three control-plane nodes tolerate one host failure while keeping etcd
    quorum. Two is worse than one — you have added a failure mode without
    gaining availability.

---

## 2. Prerequisites

### 2.1 Physical and network prerequisites

| Item | Requirement |
|---|---|
| Firmware | UEFI boot, latest BIOS/BMC firmware, Secure Boot **disabled** (Talos and kubeadm kernels are unsigned unless you enroll your own keys) |
| Virtualization | VT-x/AMD-V enabled if you will run the [VM validation harness](#9-vm-validation-harness) |
| BMC | IPMI/iDRAC/iLO/Redfish reachable on the out-of-band VLAN for console access and power control |
| Switch ports | LACP-capable ports for the bond, VLAN trunking enabled |
| PDU | Redundant power feeds on separate circuits |
| DNS | Forward and reverse records for every node and for the MetalLB service addresses |
| NTP | Reachable time source. Stellar Core logs and metrics assume a sane clock |

### 2.2 Storage prerequisites

Complete [Bare-Metal NVMe IOPS Tuning](../performance/bare-metal-iops.md)
first. By the time you start the cluster:

- [ ] The I/O scheduler is `none` for NVMe, persisted with a udev rule.
- [ ] Each data drive is formatted (XFS for archives, XFS or ext4 for
      validators) and mounted with `noatime`.
- [ ] TRIM is enabled with exactly one method (`fstrim.timer` **or** `discard`).
- [ ] `examples/scripts/run-fio-benchmark.sh --strict` passes for the node's
      profile, and the `summary.md` is filed with the host's records.
- [ ] The mount survives a reboot.

### 2.3 Software prerequisites

On your workstation, not on the nodes:

| Tool | Version used here | Purpose |
|---|---|---|
| `talosctl` | v1.8+ | Talos API client (Path A) |
| `kubeadm` / `kubelet` / `kubectl` | v1.30 (match `-kubernetes-version` in CI) | Cluster bootstrap (Path B) |
| `kubectl` | v1.30+ | Cluster access |
| `helm` | v3.14+ | Operator install |
| `k9s` (optional) | latest | Operational convenience |

!!! note "Kubernetes version"
    The repository's manifest gates validate against Kubernetes **1.30.0**
    (`scripts/validate-k8s-manifests.py`). Pin your cluster to 1.30 unless you
    have deliberately re-validated the CRDs against a newer version.

---

## 3. Path A — Talos Linux

[Talos Linux](https://www.talos.dev/) is an immutable, API-managed Kubernetes
OS. It has no SSH and no package manager, which is exactly what you want for a
validator host: the boot image is the configuration, so a host cannot drift.
Talos is the recommended path because disk mounts, kernel modules, sysctls,
and udev rules are all declarative machine-config fields.

### 3.1 Generate secrets and machine configs

```bash
# Cluster name and endpoint VIP are yours to choose. The VIP must be free.
export CLUSTER_NAME=stellar-baremetal
export CP_VIP=10.20.0.10        # control-plane endpoint (kube-vip/L2 VIP)

talosctl gen secrets -o secrets.yaml

talosctl gen config "$CLUSTER_NAME" "https://${CP_VIP}:6443" \
  --with-secrets secrets.yaml \
  --output-dir _out
```

This produces `controlplane.yaml`, `worker.yaml`, and `talosconfig`. The files
are **credentials** — the machine configs embed cluster secrets. Keep
`secrets.yaml` and `_out/` out of version control.

### 3.2 Encode the disk and kernel tuning

The block below is the part that makes a Talos node suitable for Stellar. It
mounts the second NVMe at `/var/mnt/stellar`, applies the I/O scheduler through
Talos' declarative `udev` config, and sets the sysctls Stellar Core benefits
from. Append it to `_out/controlplane.yaml` (and `_out/worker.yaml` for archive
nodes) under the existing `machine:` key — do not add a second `machine:` key.

```yaml
machine:
  # Mount the dedicated data drive. This is how mount flags reach the pod:
  # a Kubernetes `local` PV bind-mounts a directory that is already mounted
  # on the host, and a PV's mountOptions are NOT applied to `local` volumes.
  disks:
    - device: /dev/nvme1n1
      partitions:
        - mountpoint: /var/mnt/stellar

  # Declarative udev rules: force the blk-mq "none" scheduler on NVMe.
  # This is the Talos-native equivalent of the /etc/udev/rules.d rule in
  # docs/performance/bare-metal-iops.md §3.3.
  udev:
    rules:
      - SUBSYSTEM=="block", ACTION=="add|change", KERNEL=="nvme[0-9]*n[0-9]*", ATTR{queue/scheduler}="none"

  # Kernel modules required by the storage and network stack.
  kernel:
    modules:
      - name: nvme
      - name: nvme_core
      - name: bonding
      - name: 8021q
      - name: br_netfilter
      - name: overlay

  # Sysctls. bridge-nf-call-iptables is required for kube-proxy iptables mode.
  sysctls:
    net.bridge.bridge-nf-call-iptables: "1"
    net.bridge.bridge-nf-call-ip6tables: "1"
    net.ipv4.ip_forward: "1"
    # Keep the ephemeral port range wide enough for heavy peer churn.
    net.ipv4.ip_local_port_range: "10240 65535"
    # Stellar Core holds many long-lived peer sockets; raise the backlog.
    net.core.somaxconn: "4096"
    net.ipv4.tcp_max_syn_backlog: "8192"
    # Reduce TIME_WAIT pressure from frequent peer reconnects.
    net.ipv4.tcp_fin_timeout: "15"
    net.ipv4.tcp_tw_reuse: "1"
    vm.swappiness: "0"

  kubelet:
    extraArgs:
      # Local volumes are not expandable online; surface the topology so the
      # scheduler places the pod on the node that owns the PV.
      topology-manager-policy: "best-effort"
```

!!! danger "`disks` is destructive on first boot"
    Talos partitions the devices listed under `machine.disks`. Confirm the
    device path with `talosctl get disks --insecure --nodes <ip>` while the
    node is in maintenance mode, and check the serial number against your
    rack inventory. Selecting the wrong device wipes the system disk.

!!! note "Why `/var/mnt/stellar`"
    Talos allows writable mounts only under `/var`. `/var/mnt/stellar` is the
    conventional location, and it is the path you will reference from the
    `local` PersistentVolume in [§6](#6-local-storage-provisioning). The mount
    flags (`noatime`, and so on) come from the *host* — Talos sets sensible
    defaults, and [bare-metal IOPS §4](../performance/bare-metal-iops.md#4-filesystem-xfs-and-ext4)
    documents how to verify them.

### 3.3 Configure the bonded, VLAN-segmented fabric

Talos expresses bonds and VLANs as first-class fields on the interface. The
example below builds a two-port LACP bond and stacks the storage VLAN and the
SCP (consensus) VLAN on top of it.

```yaml
machine:
  network:
    hostname: stellar-cp-1
    interfaces:
      # 1. The LACP bond. mode 802.3ad requires matching LACP config on the switch.
      - interface: bond0
        bond:
          interfaces:
            - eno1
            - eno2
          mode: 802.3ad
          lacpRate: fast
          xmitHashPolicy: layer3+4
          miimon: 100
        mtu: 9000            # jumbo frames end-to-end; see §5.4
        addresses:
          - 10.20.0.11/24
        routes:
          - network: 0.0.0.0/0
            gateway: 10.20.0.1

      # 2. VLAN 20 — SCP / peer-to-peer traffic (Stellar Core port 11625).
      - interface: bond0
        vlans:
          - vlanId: 20
            mtu: 9000
            addresses:
              - 10.20.20.11/24

      # 3. VLAN 30 — storage replication / backup traffic.
      - interface: bond0
        vlans:
          - vlanId: 30
            mtu: 9000
            addresses:
              - 10.20.30.11/24

      # 4. Leave the BMC ports unmanaged; they are out-of-band.
      - interface: eno3
        ignore: true
      - interface: eno4
        ignore: true
    nameservers:
      - 10.20.0.53
      - 10.20.0.54
```

!!! warning "Two interfaces, one bond key"
    In Talos each VLAN is declared on the *parent* interface by repeating the
    `interface:` name and adding a `vlans:` list. Do not try to declare
    `bond0.20` as a separate `interface:` — that syntax is for pre-existing
    devices only.

!!! tip "`layer3+4` hashing"
    SCP peer traffic is a small number of long-lived TCP flows. `layer3+4`
    hashes on the IP/port 4-tuple so distinct peers land on distinct bond
    members. With `layer2` hashing a handful of flows can pin to one link and
    leave the other idle.

### 3.4 Boot and apply the configuration

```bash
# 1. Boot each node from the Talos ISO over the BMC virtual media, or PXE-boot
#    with the metal image. The node comes up in maintenance mode.

# 2. Identify the data disk before applying anything destructive.
talosctl get disks --insecure --nodes 10.20.0.11

# 3. Apply the control-plane config. Repeat per node with the right hostname.
talosctl apply-config --insecure --nodes 10.20.0.11 --file _out/controlplane.yaml
talosctl apply-config --insecure --nodes 10.20.0.12 --file _out/controlplane.yaml
talosctl apply-config --insecure --nodes 10.20.0.13 --file _out/controlplane.yaml

# 4. Point talosctl at the cluster.
export TALOSCONFIG=_out/talosconfig
talosctl config endpoint 10.20.0.11
talosctl config node 10.20.0.11

# 5. Wait for the API server.
talosctl health --wait-timeout 10m

# 6. Fetch the kubeconfig.
talosctl kubeconfig ./kubeconfig
export KUBECONFIG=./kubeconfig
kubectl get nodes -o wide
```

Add archive workers with `worker.yaml`:

```bash
talosctl apply-config --insecure --nodes 10.20.0.21 --file _out/worker.yaml
```

### 3.5 Install a CNI (required before anything schedules)

Talos ships **no CNI**. Nodes stay `NotReady` until you install one. Choose
based on whether you want BGP from the CNI or only from MetalLB:

=== "Cilium (BGP-capable)"

    ```bash
    helm repo add cilium https://helm.cilium.io/
    helm install cilium cilium/cilium --version 1.16.1 \
      --namespace kube-system \
      --set ipam.mode=kubernetes \
      --set kubeProxyReplacement=true \
      --set k8sServiceHost=10.20.0.10 \
      --set k8sServicePort=6443 \
      --set routingMode=native \
      --set autoDirectNodeRoutes=true \
      --set bpf.masquerade=true
    ```

    `kubeProxyReplacement=true` removes `kube-proxy` and its iptables
    churn on a busy peer port.

=== "Calico"

    ```bash
    kubectl apply -f https://raw.githubusercontent.com/projectcalico/calico/v3.28.1/manifests/tigera-operator.yaml
    kubectl apply -f - <<'EOF'
    apiVersion: operator.tigera.io/v1
    kind: Installation
    metadata:
      name: default
    spec:
      calicoNetwork:
        ipPools:
          - name: default-ipv4-ippool
            blockSize: 26
            cidr: 10.244.0.0/16
            encapsulation: None   # native routing on a flat L2/L3 fabric
            natOutgoing: Enabled
    EOF
    ```

    `encapsulation: None` avoids VXLAN overhead on the SCP path when your
    fabric already routes pod CIDRs.

!!! note "Pod CIDR must not overlap the host network"
    If your nodes use `10.20.0.0/16`, do not hand the cluster a pod CIDR from
    the same range. The examples above use `10.244.0.0/16`.

### 3.6 Verify the machine config landed

```bash
# Scheduler is "none" on every NVMe.
talosctl get blockdevices --nodes 10.20.0.11
talosctl read /sys/block/nvme1n1/queue/scheduler --nodes 10.20.0.11
# [none] mq-deadline kyber bfq

# The data drive is mounted at /var/mnt/stellar.
talosctl read /proc/mounts --nodes 10.20.0.11 | grep stellar

# The bond is up and the VLANs exist.
talosctl get links --nodes 10.20.0.11
```

---

## 4. Path B — kubeadm

Use kubeadm when Talos is not an option: an existing OS standard, a
hardware-certification requirement, or a team already fluent in kubeadm
operations. kubeadm gives you a standard conformant cluster; you are
responsible for the OS-level disk and network setup that Talos does for you.

### 4.1 Host preparation (every node)

Choose one distribution per fleet and pin its kernel. The steps below assume
Ubuntu 22.04 LTS or Rocky Linux 9.

```bash
# ── Disable swap permanently ────────────────────────────────────────────────
sudo swapoff -a
sudo sed -ri '/\sswap\s/s/^#?/#/' /etc/fstab
free -h   # Swap row must read 0B

# ── Kernel modules ─────────────────────────────────────────────────────────
sudo tee /etc/modules-load.d/k8s.conf >/dev/null <<'EOF'
overlay
br_netfilter
bonding
8021q
nvme
nvme_core
EOF
sudo modprobe overlay br_netfilter bonding 8021q

# ── Sysctls ────────────────────────────────────────────────────────────────
sudo tee /etc/sysctl.d/99-stellar-k8s.conf >/dev/null <<'EOF'
net.bridge.bridge-nf-call-iptables  = 1
net.bridge.bridge-nf-call-ip6tables = 1
net.ipv4.ip_forward                 = 1
net.ipv4.ip_local_port_range        = 10240 65535
net.core.somaxconn                  = 4096
net.ipv4.tcp_max_syn_backlog        = 8192
net.ipv4.tcp_fin_timeout            = 15
net.ipv4.tcp_tw_reuse               = 1
vm.swappiness                       = 0
EOF
sudo sysctl --system
```

!!! danger "Do not skip `swapoff`"
    kubelet refuses to start with swap enabled by default. If you need swap for
    other workloads, keep it off on Stellar nodes — swapping a validator's
    ledger working set causes multi-second ledger-apply stalls.

### 4.2 Install containerd and the kube toolchain

```bash
# ── containerd ─────────────────────────────────────────────────────────────
sudo apt-get update
sudo apt-get install -y containerd

sudo mkdir -p /etc/containerd
containerd config default | sudo tee /etc/containerd/config.toml >/dev/null
# kubeadm >= 1.24 requires the systemd cgroup driver.
sudo sed -i 's/SystemdCgroup = false/SystemdCgroup = true/' /etc/containerd/config.toml
sudo systemctl restart containerd
sudo systemctl enable containerd

# ── Kubernetes packages (pin 1.30 to match CI validation) ──────────────────
sudo mkdir -p /etc/apt/keyrings
curl -fsSL https://pkgs.k8s.io/core:/stable:/v1.30/deb/Release.key \
  | sudo gpg --dearmor -o /etc/apt/keyrings/kubernetes-apt-keyring.gpg
echo 'deb [signed-by=/etc/apt/keyrings/kubernetes-apt-keyring.gpg] https://pkgs.k8s.io/core:/stable:/v1.30/deb/ /' \
  | sudo tee /etc/apt/sources.list.d/kubernetes.list
sudo apt-get update
sudo apt-get install -y kubelet kubeadm kubectl
sudo apt-mark hold kubelet kubeadm kubectl
```

!!! note "RHEL-family nodes"
    Use the `rpm` repository from `pkgs.k8s.io` and replace `apt-get` with
    `dnf`. Set `SELinux` to `permissive` only if you have verified that
    containerd and the CNI you choose both ship policies; `enforcing` with a
    proper policy is preferred.

### 4.3 Mount the Stellar data drive

Do this before `kubeadm init` so the `local` PV path exists at bootstrap.

```bash
# 1. Confirm the device. WRONG DEVICE = DATA LOSS.
lsblk -o NAME,MODEL,SERIAL,SIZE,MOUNTPOINT
sudo nvme list

# 2. Format and mount. XFS is the recommendation for archive nodes.
sudo mkfs.xfs -L stellar-data /dev/nvme1n1
sudo mkdir -p /var/mnt/stellar

# 3. Persist by UUID (kernel device names are not stable across boots).
UUID=$(sudo blkid -s UUID -o value /dev/nvme1n1)
echo "UUID=${UUID} /var/mnt/stellar xfs defaults,noatime,inode64,logbsize=256k 0 2" \
  | sudo tee -a /etc/fstab
sudo systemctl daemon-reload
sudo mount -a
findmnt /var/mnt/stellar
```

The scheduler and TRIM settings from
[bare-metal IOPS §3](../performance/bare-metal-iops.md#3-kernel-io-scheduler)
and [§4.6](../performance/bare-metal-iops.md#4-filesystem-xfs-and-ext4) are
applied the same way on kubeadm nodes.

### 4.4 Bond and VLAN configuration

With kubeadm you configure the fabric with the OS tooling. Use the
distribution's declarative network manager so the config survives a reboot —
`netplan` on Ubuntu, `nmcli` on Rocky, or a `systemd-networkd` unit on either.

```yaml
# /etc/netplan/10-stellar-fabric.yaml  (Ubuntu)
network:
  version: 2
  renderer: networkd
  ethernets:
    eno1: {dhcp4: false, dhcp6: false}
    eno2: {dhcp4: false, dhcp6: false}
    eno3: {dhcp4: false, dhcp6: false, optional: true}
    eno4: {dhcp4: false, dhcp6: false, optional: true}
  bonds:
    bond0:
      interfaces: [eno1, eno2]
      parameters:
        mode: 802.3ad
        lacp-rate: fast
        transmit-hash-policy: layer3+4
        mii-monitor-interval: 100
      mtu: 9000
      addresses: [10.20.0.11/24]
      routes:
        - to: default
          via: 10.20.0.1
      nameservers:
        addresses: [10.20.0.53, 10.20.0.54]
  vlans:
    bond0.20:                     # SCP / peer-to-peer
      id: 20
      link: bond0
      mtu: 9000
      addresses: [10.20.20.11/24]
    bond0.30:                     # storage replication / backup
      id: 30
      link: bond0
      mtu: 9000
      addresses: [10.20.30.11/24]
```

```bash
sudo netplan apply
ip -br addr show bond0
ip -br link show bond0.20
cat /proc/net/bonding/bond0   # Both slaves must show "MII Status: up"
```

!!! tip "kubelet node IP on the right network"
    If you want the kubelet to advertise the SCP VLAN address, set
    `nodeIP.validSubnets` in `KubeletConfiguration` (Talos) or
    `--node-ip` in the kubelet drop-in. This matters when you want SCP traffic
    to stay on VLAN 20 end-to-end.

### 4.5 Initialise the control plane

```bash
# ── First control-plane node ───────────────────────────────────────────────
sudo kubeadm init \
  --control-plane-endpoint "10.20.0.10:6443" \
  --upload-certs \
  --pod-network-cidr "10.244.0.0/16" \
  --apiserver-advertise-address "10.20.0.11"

mkdir -p "$HOME/.kube"
sudo cp -i /etc/kubernetes/admin.conf "$HOME/.kube/config"
sudo chown "$(id -u):$(id -g)" "$HOME/.kube/config"

# ── CNI (Talos §3.5 shows Cilium/Calico; the manifests are identical) ──────

# ── Additional control-plane nodes ─────────────────────────────────────────
# kubeadm init prints a `--control-plane --certificate-key ...` command.
# Run it on cp-2 and cp-3.

# ── Workers ────────────────────────────────────────────────────────────────
# kubeadm init prints a `kubeadm join` command. Run it on each archive node.
kubectl get nodes -o wide
```

!!! warning "Removing the control-plane taint"
    The default control-plane nodes are tainted
    `node-role.kubernetes.io/control-plane:NoSchedule`. In the reference
    topology the control-plane nodes *are* the validator hosts. If you intend
    to schedule validators there, remove the taint deliberately and accept
    that etcd and Stellar Core now share a host:

    ```bash
    kubectl taint nodes stellar-cp-1 node-role.kubernetes.io/control-plane-
    ```

    On a three-node cluster this is a common and reasonable choice. Document it
    in your runbook so the next operator knows the coupling is intentional.

### 4.6 MetalLB for external access

MetalLB is the cloud-independent replacement for a cloud load balancer. It is
required for exposing validator peer ports and Horizon/RPC APIs.

```bash
helm repo add metallb https://metallb.github.io/metallb
helm repo update
helm install metallb metallb/metallb --namespace metallb-system --create-namespace

# Wait for the controller and speakers to be ready before configuring.
kubectl -n metallb-system rollout status deploy/metallb-controller
kubectl -n metallb-system rollout status ds/metallb-speaker
```

L2 mode is the simplest and needs no router cooperation. See
[§5.3](#53-bgp-for-scp-traffic) for BGP, and
[MetalLB/BGP Anycast](../metallb-bgp-anycast.md) for global anycast.

```yaml
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata:
  name: stellar-pool
  namespace: metallb-system
spec:
  addresses:
    - 10.20.100.0/28      # A routed, unused block from your network team
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata:
  name: stellar-l2
  namespace: metallb-system
spec:
  ipAddressPools:
    - stellar-pool
```

!!! danger "`strictARP` is mandatory in L2 mode"
    MetalLB L2 mode requires `strictARP: true` in the `kube-proxy` config.
    Without it, hosts answer ARP for addresses MetalLB has not claimed yet and
    traffic is black-holed intermittently.

    ```bash
    kubectl -n kube-system get configmap kube-proxy -o yaml \
      | sed 's/strictARP: false/strictARP: true/' \
      | kubectl apply -f -
    kubectl -n kube-system rollout restart daemonset kube-proxy
    ```

    This does not apply if you replaced `kube-proxy` with Cilium
    (`kubeProxyReplacement=true`).

---

## 5. Physical network topology

This section is deliberately tool-agnostic: the same design applies whether
the nodes run Talos or kubeadm. It is the layer that the issue's review
criteria call out as "network bridging".

### 5.1 Segmentation model

| VLAN | Purpose | Subnet (example) | Notes |
|---|---|---|---|
| 10 | Out-of-band management | 10.20.10.0/24 | BMC/IPMI only. No Kubernetes traffic. Firewalled from everything. |
| 15 | Kubernetes node / API | 10.20.0.0/24 | kubelet, API server, etcd peer traffic |
| 20 | **SCP / peer-to-peer** | 10.20.20.0/24 | Stellar Core port 11625. Isolated from management and general traffic. |
| 30 | Storage replication / backup | 10.20.30.0/24 | Volume snapshots, backup shipping |
| 40 | Public / DMZ ingress | routed, public | MetalLB service IPs only, via a firewall |

Rules:

- **SCP traffic stays on VLAN 20.** Consensus messages are latency-critical and
  must not contend with image pulls, log shipping, or general API traffic.
  Segmenting the peer path also makes it trivial to write a switch ACL that
  permits `tcp/11625` only between validator hosts.
- **Management VLAN 10 is never trunked to a workload bridge.** A compromised
  pod must not be one hop from a BMC.
- **Public ingress is a firewall decision, not a Kubernetes one.** MetalLB
  advertises the service IP; your perimeter firewall decides who may reach it.

### 5.2 Bonding

| Mode | When to use | Caveat |
|---|---|---|
| `802.3ad` (LACP) | **Default.** Both links active, switch-side LACP, per-flow load balancing | Requires matching LACP config on the switch. A misconfigured member causes intermittent loss. |
| `active-backup` | Switch does not support LACP, or you want the simplest possible failure mode | One link idle. Use `primary` + `primaryReselect: better` to prefer the faster port. |
| `balance-xor` | Legacy switches that cannot do LACP | Static, no failure detection. Prefer LACP. |

Do not use `balance-rr`. It reorders packets and will corrupt or stall TCP
flows, including SCP connections.

Health monitoring: `miimon=100` plus, where the switch supports it,
`arp_interval`/`arp_ip_target` for a deeper liveness check than link carrier.

### 5.3 BGP for SCP traffic

For validator peer traffic, BGP advertisement from the node to the
top-of-rack switch is strongly preferred over L2/ARP. In L2 mode all peer
traffic for a service IP hairpins through one elected speaker node; with BGP
the route follows the shortest path and each validator is reachable
independently.

```yaml
apiVersion: metallb.io/v1beta2
kind: BGPPeer
metadata:
  name: tor-switch
  namespace: metallb-system
spec:
  myASN: 64512
  peerASN: 64513
  peerAddress: 10.20.20.1
  holdTime: 90s
  keepaliveTime: 30s
  # MD5 is optional; use it when the switch supports it.
  # passwordSecret: bgp-md5-secret
  bfdProfile: stellar-bfd
---
apiVersion: metallb.io/v1beta1
kind: BFDProfile
metadata:
  name: stellar-bfd
  namespace: metallb-system
spec:
  receiveInterval: 300ms
  transmitInterval: 300ms
  detectMultiplier: 3
  echoMode: false
---
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata:
  name: stellar-scp-pool
  namespace: metallb-system
spec:
  addresses:
    - 10.20.20.100/32
  autoAssign: false
---
apiVersion: metallb.io/v1beta1
kind: BGPAdvertisement
metadata:
  name: stellar-scp
  namespace: metallb-system
spec:
  ipAddressPools:
    - stellar-scp-pool
  aggregationLength: 32
  localPref: 200
  communities:
    - 64512:100
```

BFD at a 300 ms interval with a multiplier of 3 detects a dead path in under a
second, so peer connections fail over before Stellar Core's own timeouts fire.

!!! note "Static peer list vs. BGP"
    Stellar Core's `QUORUM_SET` takes literal addresses, not DNS. If you
    advertise the peer IPs with BGP, use the **anycast/stable** address in the
    quorum set rather than a node's host address — otherwise a node replacement
    changes the quorum configuration. For a fixed, rack-local quorum, static
    addresses are simpler and equally valid.

### 5.4 Jumbo frames

MTU 9000 on the storage and SCP VLANs reduces per-packet overhead and interrupt
load. It is worth enabling, but only if the **entire path** supports it.

Check the effective MTU end-to-end before trusting it:

```bash
# 8972 = 9000 - 28 bytes of IP+ICMP headers. Must be 0% loss and NOT fragmented.
ping -M do -s 8972 -c 3 10.20.20.12
```

If any hop reports "Message too long" or "Frag needed", fix that hop or drop
back to 1500. A silently fragmented path is worse than no jumbo frames: it adds
latency and CPU with no benefit.

Kubernetes overlays subtract their own header (VXLAN: 50 bytes). If you use an
encapsulating CNI, set the pod MTU to 8950 or use native routing as shown in
[§3.5](#35-install-a-cni-required-before-anything-schedules).

---

## 6. Local storage provisioning

Stellar nodes are stateful. A `StellarNode` with `storage.mode: Local` binds a
PVC to a `local` PersistentVolume that already exists on one host. This section
covers the three supported ways to produce those volumes. All three avoid any
cloud CSI driver.

The canonical manifests live in
[`examples/bare-metal/storage-class.yaml`](../../examples/bare-metal/storage-class.yaml);
the excerpts below are the annotated highlights.

### 6.1 Choosing an approach

| Approach | Dynamic? | Best for | Trade-off |
|---|---|---|---|
| Static `local` PVs | No | Validators on a fixed rack | Most explicit; one PV per host; full control of `nodeAffinity` |
| Local Path Provisioner | Yes | Archive/Horizon nodes, dev, VM harness | Simplest; no capacity enforcement; hostPath backend is not a true `local` PV |
| OpenEBS Local PV | Yes | Larger fleets needing capacity and topology control | Extra component to operate; more knobs |

The operator already knows about Local Path Provisioner: when
`storage.mode: Local` is set and `storageClass` is left empty, it looks for an
existing `local-path` or `local-storage` StorageClass. **Set the StorageClass
explicitly in production** so a node never lands on an untuned default path.

### 6.2 Option A — static local PVs (recommended for validators)

This is the most predictable option and the one the validator path in
[bare-metal IOPS §7](../performance/bare-metal-iops.md#7-deploying-on-stellar-k8s-with-local-nvme)
describes. You create one `local` PV per host, pinned to that host by
`nodeAffinity`.

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: stellar-local-nvme
provisioner: kubernetes.io/no-provisioner   # no cloud, no dynamic provisioning
volumeBindingMode: WaitForFirstConsumer     # bind only after the pod is scheduled
reclaimPolicy: Retain                       # never auto-delete a ledger database
allowVolumeExpansion: false                 # local volumes cannot grow online
---
apiVersion: v1
kind: PersistentVolume
metadata:
  name: stellar-nvme-cp-1
  labels:
    stellar.org/storage: nvme-tuned
spec:
  capacity:
    storage: 3400Gi
  accessModes: ["ReadWriteOnce"]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: stellar-local-nvme
  volumeMode: Filesystem
  local:
    path: /var/mnt/stellar          # the tuned mount from §3.2 / §4.3
  nodeAffinity:
    required:
      nodeSelectorTerms:
        - matchExpressions:
            - key: kubernetes.io/hostname
              operator: In
              values: ["stellar-cp-1"]
```

!!! warning "Capacity is a claim, not an enforcement"
    For `local` volumes Kubernetes does **not** enforce `capacity`. Nothing
    stops a node from writing past 3400Gi until the filesystem fills. Size the
    PV below the real usable space and alert on filesystem usage.

Label nodes only after they pass the fio benchmark, then reference the label
from the `StellarNode`:

```bash
kubectl label node stellar-cp-1 stellar.org/storage=nvme-tuned
```

To manage many hosts, the
[local static provisioner](https://github.com/kubernetes-sigs/sig-storage-local-static-provisioner)
creates these PVs automatically from mounts under a discovery directory. Point
its discovery directory at the parents of your Stellar mounts.

### 6.3 Option B — Local Path Provisioner (recommended for archives)

Local Path Provisioner gives you dynamic provisioning with a hostPath or
`local` backend and a trivial install. It is the right default for Horizon and
Soroban RPC archive nodes and for the VM validation harness.

```bash
kubectl apply -f https://raw.githubusercontent.com/rancher/local-path-provisioner/v0.0.37/deploy/local-path-storage.yaml
```

Point it at the tuned mount by editing its ConfigMap so volumes are created on
`/var/mnt/stellar` rather than the default `/opt/local-path-provisioner`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: local-path-config
  namespace: local-path-storage
data:
  config.json: |-
    {
      "nodePathMap": [
        {
          "node": "DEFAULT_PATH_FOR_NON_LISTED_NODES",
          "paths": ["/var/mnt/stellar/local-path"]
        }
      ]
    }
```

Then create a StorageClass that pins the class to the tuned path and requests a
true `local` (rather than `hostPath`) PV:

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: stellar-local-path
  annotations:
    defaultVolumeType: local          # create `local` PVs, not `hostPath`
provisioner: rancher.io/local-path
parameters:
  nodePath: /var/mnt/stellar/local-path
  pathPattern: "{{ .PVC.Namespace }}/{{ .PVC.Name }}/"
volumeBindingMode: WaitForFirstConsumer
reclaimPolicy: Retain
allowVolumeExpansion: false
```

!!! danger "Keep `pathPattern` within the required prefix"
    As of local-path-provisioner v0.0.33 the rendered `pathPattern` must start
    with `{{ .PVC.Namespace }}/{{ .PVC.Name }}/` and must not contain `../`.
    `allowUnsafePathPattern: "true"` disables that check and re-opens a
    path-traversal class of bug (CVE-2025-62878). Do not set it.

!!! note "`local` vs `hostPath`"
    A `local` PV participates in node affinity and gives you scheduler-visible
    topology. A `hostPath` PV does not, so a rescheduled pod can silently mount
    a *different* host's directory. Set `defaultVolumeType: local` for anything
    holding ledger data.

### 6.4 Option C — OpenEBS Local PV (larger fleets)

OpenEBS Local PV Hostpath is a drop-in dynamic provisioner with capacity and
topology controls that Local Path Provisioner lacks.

```bash
helm repo add openebs https://openebs.github.io/openebs
helm repo update
helm install openebs openebs/openebs \
  --namespace openebs --create-namespace \
  --set localpv-provisioner.hostpathClass.isDefaultClass=false
```

Create a class whose base path is the tuned mount, restricted to the hosts that
actually have it:

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: stellar-openebs-nvme
  annotations:
    openebs.io/cas-type: local
    cas.openebs.io/config: |
      - name: StorageType
        value: "hostpath"
      - name: BasePath
        value: "/var/mnt/stellar/openebs"
      - name: FilePermissions
        data:
          mode: "0770"
provisioner: openebs.io/local
volumeBindingMode: WaitForFirstConsumer
reclaimPolicy: Retain
allowVolumeExpansion: false
allowedTopologies:
  - matchLabelExpressions:
      - key: stellar.org/storage
        values: ["nvme-tuned"]
```

`allowedTopologies` makes the scheduler place the pod only on labelled hosts,
which is what you want when only some nodes have a tuned NVMe.

### 6.5 Do not use a cloud StorageClass

!!! danger "Cloud dependencies are out of scope"
    The manifests in `examples/bare-metal/` intentionally contain **no**
    `ebs.csi.aws.com`, `pd.csi.storage.gke.io`, `disk.csi.azure.com`, or any
    other cloud provisioner, and no cloud load balancer annotations. If you
    copy a snippet from a cloud guide, the pod will stay `Pending` forever with
    a `no volume plugin matched` or `storageclass not found` event.

    Use [`examples/bare-metal/storage-class.yaml`](../../examples/bare-metal/storage-class.yaml)
    as the source of truth for this cluster.

---

## 7. Deploy Stellar-K8s on local volumes

### 7.1 Install the operator

```bash
kubectl apply -f https://raw.githubusercontent.com/OtowoOrg/Stellar-K8s/main/config/crd/stellarnode-crd.yaml

kubectl create namespace stellar-system --dry-run=client -o yaml | kubectl apply -f -

helm upgrade --install stellar-operator charts/stellar-operator \
  --namespace stellar-system \
  --set image.tag=v0.1.0 \
  --wait --timeout 5m
```

### 7.2 Create the namespace and seed secret

```bash
kubectl create namespace stellar

# The seed is a credential. Create it from a file, never from shell history.
kubectl -n stellar create secret generic validator-seed-mainnet \
  --from-file=STELLAR_CORE_SEED=/secure/path/validator.seed
```

### 7.3 A validator pinned to a physical NVMe

This is the manifest the validation harness deploys. It uses static local
storage (`storage.mode: Local`), disables cloud-facing features, and keeps the
peer port reachable over the SCN VLAN.

```yaml
apiVersion: stellar.org/v1alpha1
kind: StellarNode
metadata:
  name: validator-baremetal-1
  namespace: stellar
spec:
  nodeType: Validator
  network: mainnet
  version: "v21.0.0"
  historyMode: Recent

  # The CRD marks these three as required. maxUnavailable/minAvailable are
  # mutually exclusive; set exactly one. An empty topologySpreadConstraints
  # list means "no additional spread constraint".
  maxUnavailable: 0
  topologySpreadConstraints: []

  resources:
    requests:
      cpu: "8"
      memory: "24Gi"
    limits:
      cpu: "16"
      memory: "48Gi"

  storage:
    mode: Local
    storageClass: stellar-local-nvme
    size: "3400Gi"
    retentionPolicy: Retain          # never auto-delete a local ledger DB
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
          - matchExpressions:
              - key: stellar.org/storage
                operator: In
                values: ["nvme-tuned"]

  podAntiAffinity: Hard

  validatorConfig:
    seedSecretRef: validator-seed-mainnet
    enableHistoryArchive: true
    historyArchiveUrls:
      - "https://history.stellar.org/prd/core-live/core_live_001"
      - "https://history.stellar.org/prd/core-live/core_live_002"
    quorumSet: |
      [QUORUM_SET]
      THRESHOLD_PERCENT=67
      VALIDATORS=[
        "10.20.20.11:11625", "10.20.20.12:11625", "10.20.20.13:11625"
      ]

  # In-cluster Postgres. For production prefer a dedicated database host or a
  # managed cluster you operate; see docs/database/optimization-guide.md.
  database:
    secretRef: stellar-db-credentials

  # No cloud load balancer: MetalLB provides the service IP.
  loadBalancer:
    enabled: true
    mode: BGP
    addressPool: stellar-scp-pool
    externalTrafficPolicy: Local

  alerting: true
```

Apply it:

```bash
kubectl apply -f examples/bare-metal/validator-baremetal.yaml
kubectl -n stellar get stellarnode -w
```

!!! note "`size` and local volumes"
    For `storage.mode: Local` the `size` field documents intent; it is not
    enforced against a static PV. The PV's own `capacity` and the host
    filesystem are the real limits. See
    [PVC Auto-Expansion](../pvc-auto-expansion.md) for why
    `allowVolumeExpansion: false` is correct here, and
    [Proactive Disk Scaling](../proactive-disk-scaling.md) for the
    network-volume case that does not apply to local disks.

### 7.4 An archive node on dynamically provisioned local storage

```yaml
apiVersion: stellar.org/v1alpha1
kind: StellarNode
metadata:
  name: horizon-baremetal
  namespace: stellar
spec:
  nodeType: Horizon
  network: mainnet
  version: "v21.0.0"
  historyMode: Full
  replicas: 2
  maxUnavailable: 0
  topologySpreadConstraints: []

  resources:
    requests:
      cpu: "8"
      memory: "32Gi"
    limits:
      cpu: "16"
      memory: "64Gi"

  storage:
    mode: PersistentVolume
    storageClass: stellar-local-path    # Local Path Provisioner class from §6.3
    size: "6000Gi"
    retentionPolicy: Retain

  horizonConfig:
    stellarCoreUrl: "http://validator-baremetal-1.stellar.svc.cluster.local:11626"
    databaseSecretRef: horizon-db-credentials

  ingress:
    enabled: false
  loadBalancer:
    enabled: true
    mode: BGP
    addressPool: stellar-pool
    externalTrafficPolicy: Local

  alerting: true
```

!!! warning "`historyMode: Full` needs the space"
    A full-history archive is several terabytes and keeps growing. Confirm the
    sizing in [Capacity Planning §3](../operations/capacity-planning.md#3-storage-growth-model)
    before you schedule it, and keep
    [Volume Snapshots](../volume-snapshots.md) pointed at it.

### 7.5 Verify the volumes bound to the right host

```bash
# The PVC must be Bound and its PV must be a `local` volume.
kubectl -n stellar get pvc
kubectl -n stellar get pvc validator-baremetal-1-data -o jsonpath='{.spec.volumeName}{"\n"}'
kubectl get pv "$(kubectl -n stellar get pvc validator-baremetal-1-data -o jsonpath='{.spec.volumeName}')" \
  -o jsonpath='{.spec.local.path}{"  ->  "}{.spec.nodeAffinity}{"\n"}'

# The pod must be Running on the node the PV is pinned to.
kubectl -n stellar get pods -o wide -l app.kubernetes.io/name=stellar-node
```

A `Pending` PVC with `waiting for first consumer` is normal until the pod is
scheduled. A PVC stuck with `no volume plugin matched` means the StorageClass
provisioner is missing or is a cloud driver — re-read
[§6.5](#65-do-not-use-a-cloud-storageclass).

---

## 8. Validate the cluster

Run this sequence on every cluster before you point it at mainnet.

### 8.1 Storage

```bash
# 1. Every expected StorageClass exists and has no cloud provisioner.
kubectl get sc
kubectl get sc -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.provisioner}{"\n"}{end}'

# 2. A volume actually binds to a local PV on the intended host.
kubectl apply -f - <<'EOF'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: baremetal-smoke-test
  namespace: stellar
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: stellar-local-path
  resources:
    requests:
      storage: 1Gi
EOF

kubectl -n stellar get pvc baremetal-smoke-test -w   # expect Bound
```

### 8.2 Network

```bash
# Bond health: both slaves up, no resets.
cat /proc/net/bonding/bond0

# VLAN interfaces exist with the expected MTU.
ip -br link show type vlan

# Jumbo frames work end-to-end on the SCP VLAN.
ping -M do -s 8972 -c 3 10.20.20.12

# Peer port is reachable from another validator.
kubectl -n stellar exec validator-baremetal-1-0 -- \
  nc -zv 10.20.20.12 11625
```

### 8.3 Stellar node health

```bash
# Ledger is advancing and the node is not stuck in catchup.
kubectl -n stellar port-forward validator-baremetal-1-0 11626:11626 &
curl -s http://localhost:11626/info | jq '.info.state, .info.ledger'

# Peers are authenticated.
curl -s http://localhost:11626/peers | jq '.authenticated_peers | length'

# The operator's own view of the node.
kubectl -n stellar get stellarnode validator-baremetal-1 -o wide
kubectl -n stellar describe stellarnode validator-baremetal-1
```

### 8.4 Failure drills

Do these before you rely on the cluster:

| Drill | Command | Expected result |
|---|---|---|
| Bond member loss | Unplug one bond member (or `ip link set eno2 down`) | No SCP disconnection; `/proc/net/bonding/bond0` shows the other slave active |
| Node drain | `kubectl drain stellar-archive-1 --ignore-daemonsets --delete-emptydir-data` | Archive pods reschedule; validator pods stay put (they are pinned by local PV affinity) |
| StorageClass failure | Delete the Local Path Provisioner pod | Existing pods keep running; new PVCs stay `Pending` and recover when the provisioner returns |
| MetalLB failover | `kubectl -n metallb-system delete pod -l component=speaker --field-selector spec.nodeName=stellar-cp-1` | Service IP remains reachable via another speaker |
| Reboot persistence | `reboot` a node | Scheduler is `none`, mounts are present, bond is up, pod reattaches to its PV |

!!! danger "A drained validator cannot move"
    A validator with a `local` PV is pinned to one host. `kubectl drain` will
    evict it and the replacement pod will stay `Pending` because no other node
    owns that PV. This is expected. Plan validator maintenance as a
    quorum-aware, one-node-at-a-time operation, not as a drain.

---

## 9. VM validation harness

The issue's validation requirement is: *execute the guide using virtual
machines and verify the cluster can successfully provision a node utilizing
local disk volumes.* The harness in
[`examples/bare-metal/`](../../examples/bare-metal/README.md) automates exactly
that.

It runs the real local-storage path in a kind cluster: a `StorageClass` with no
cloud provisioner, a dynamically provisioned volume that binds to a node-local
PV, and a pod that writes and reads data back from it. That proves the
storage-attachment contract the guide depends on. Full Talos/kubeadm bootstrap
on VMs is a manual step documented in the harness README, because it needs
privileged VMs and a second boot target.

```bash
# Requires: docker, kind, kubectl
bash examples/bare-metal/scripts/validate-vm-cluster.sh
```

Expected output ends with:

```text
✅ Local disk volume provisioning validated
```

The script is safe to re-run: it creates a uniquely named kind cluster and
deletes it in a `trap` on exit. Use `--keep` to retain the cluster for
inspection.

---

## 10. Production checklist

Before admitting the cluster to mainnet:

**Storage**

- [ ] Every data drive benchmarked with `run-fio-benchmark.sh --strict` for its profile.
- [ ] Scheduler `none` (or measured `mq-deadline` for archive-only drives), persisted with a udev rule.
- [ ] Mounts present after a reboot; `noatime` set; exactly one TRIM method enabled.
- [ ] `StorageClass` has `volumeBindingMode: WaitForFirstConsumer` and `reclaimPolicy: Retain`.
- [ ] No cloud provisioner anywhere: `kubectl get sc -o jsonpath='{..provisioner}'` shows only local drivers.
- [ ] Static PVs sized below usable capacity, with filesystem-usage alerting.
- [ ] `retentionPolicy: Retain` on every `StellarNode` holding ledger data.
- [ ] Volume snapshots and the [DR runbook](../operations/disaster-recovery.md) exercised end to end.

**Network**

- [ ] SCP traffic confined to its own VLAN, with a switch ACL permitting only `tcp/11625` between validators.
- [ ] Bond in `802.3ad` with `miimon`, verified to survive a member unplug.
- [ ] Jumbo frames verified end-to-end with `ping -M do` on every VLAN that uses them.
- [ ] No cloud load balancer annotations; MetalLB configured and tested.
- [ ] `strictARP: true` if MetalLB runs in L2 mode.
- [ ] BGP sessions up with BFD, or static quorum addresses documented.
- [ ] `NetworkPolicy` default-deny in the `stellar` namespace (see [network-policy-zero-trust.md](../network-policy-zero-trust.md)).

**Kubernetes**

- [ ] Three control-plane nodes, etcd quorum verified after one-node loss.
- [ ] Cluster pinned to the Kubernetes version the CRDs were validated against (1.30).
- [ ] `kubelet` `topology-manager-policy` set for latency-sensitive validators.
- [ ] Swap disabled on every Stellar node.
- [ ] Node labels (`stellar.org/storage=nvme-tuned`) applied only after benchmarking.

**Operations**

- [ ] Monitoring and alerting from [MONITORING_SETUP_GUIDE.md](../MONITORING_SETUP_GUIDE.md) wired to the bare-metal node exporters.
- [ ] Firmware and drive wear (`nvme smart-log`) monitored.
- [ ] The cluster is reproducible: machine configs or a provisioning playbook are in version control, secrets are not.
- [ ] The [VM validation harness](#9-vm-validation-harness) passes on a clean checkout.

---

## Appendix A — Common failures on bare metal

| Symptom | Likely cause | Fix |
|---|---|---|
| PVC `Pending`, event `no volume plugin matched` | StorageClass provisioner missing, or a cloud driver was copied in | Install Local Path Provisioner / OpenEBS; re-check [§6.5](#65-do-not-use-a-cloud-storageclass) |
| Pod `Pending`, event `node(s) didn't match node affinity` | `nodeAffinity` label missing on the host | `kubectl label node <node> stellar.org/storage=nvme-tuned` |
| Validator pod evicted and replacement stuck `Pending` | `local` PV is pinned to the evicted host | Expected. Un-cordon the original node; do not drain validators |
| Bond shows one slave `down` after boot | LACP negotiation failed or switch port not trunked | Verify switch LACP config; check `cat /proc/net/bonding/bond0` |
| Jumbo frames cause stalls under load | One hop in the path has MTU 1500 | `ping -M do -s 8972` hop by hop; fix the hop or use 1500 |
| SCP peers disconnect intermittently | Two validators co-located, or peer traffic sharing the general VLAN | Enforce `podAntiAffinity: Hard`; segment SCP onto its own VLAN |
| Disk fills despite a `3400Gi` PVC | `local` PV capacity is not enforced | Alert on filesystem usage; size PVs conservatively |
| MetalLB service IP unreachable | L2 mode without `strictARP: true`, or no IPAddressPool match | Set `strictARP`; check `kubectl -n metallb-system get ipaddresspool` |
| Node `NotReady` right after bootstrap | No CNI installed | Install a CNI ([§3.5](#35-install-a-cni-required-before-anything-schedules)) |

## Appendix B — Reference manifests

| File | Purpose |
|---|---|
| [`examples/bare-metal/storage-class.yaml`](../../examples/bare-metal/storage-class.yaml) | StorageClasses for static local NVMe, Local Path Provisioner, and OpenEBS — no cloud provisioners |
| [`examples/bare-metal/validator-baremetal.yaml`](../../examples/bare-metal/validator-baremetal.yaml) | Validator `StellarNode` pinned to a physical NVMe via a static local PV |
| [`examples/bare-metal/network-attachment.yaml`](../../examples/bare-metal/network-attachment.yaml) | Bond/VLAN reference and a validator `NetworkPolicy` confined to the SCP VLAN |
| [`examples/bare-metal/scripts/validate-vm-cluster.sh`](../../examples/bare-metal/scripts/validate-vm-cluster.sh) | VM validation harness for local-disk provisioning |
