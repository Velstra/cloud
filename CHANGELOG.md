# Changelog

All notable changes to Velstra Cloud are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and from `0.1.0` the project
follows [Semantic Versioning](https://semver.org/).

**A released version is not the version apt compares.** A `.deb` built from any
commit calls itself `<version>+<timestamp>.<revision>`, because a package whose
version never moves is a package a cell cannot be upgraded with — `apt install
./new.deb` answers "already the newest version" and does nothing. The timestamp
leads because only it is monotonic; the revision says exactly which build this
is. A tag makes that revision a real one instead of `dirty`.

## [Unreleased]

### Added

- **Retryable creates.** `Idempotency-Key` on every create: the second attempt
  is answered with the first attempt's operation and target, having made
  nothing new. The request is fingerprinted, so a key reused for a *different*
  create is refused rather than answered with somebody else's object; a claim
  is written before the work, so two attempts arriving together cannot both do
  it. Keys are remembered for a day and swept. A create that lets the platform
  pick the name had no handle at all: the request that timed out may have
  landed, and the only way to find out was to look — the race the retry was
  trying to avoid.
- **An image has a life stage.** In service, deprecated, retired, with a
  replacement named. A family resolves past a deprecated image while a pinned
  digest keeps working; a retired one is refused for anything new and does not
  touch what is already running. Publishing a newer image deprecates the one it
  supersedes and points forward. The refusal names where to go.
- **What a guest is using.** CPU time, resident memory, bytes and packets
  across its wires, and — from the VMM's own monitor — what its disks have
  moved. Counters for a monitoring system, one CPU rate for a person, computed
  over the interval rather than since boot. Reported on a five-minute cadence
  of its own: every counter moves on every pass, and a reading per resync would
  be a status write per guest per pass.
- **Network traffic on the bill.** Each hourly usage reading carries the bytes
  that crossed the project's wires during it, and `:explainUsage` sums them.
  The node carries each guest's totals across a tap that was remade, so a
  restart or a migration no longer loses everything since the last reading.
- **Four more quota dimensions** — snapshots, backups and the gibibytes they
  hold. `:explainUsage` now bills every dimension the platform counts,
  including load-balancer and device hours.
- **Load-balancer health checking.** The node opens a two-second connection to
  exactly the ports a balancer names and reports what answered; the controller
  programs the members that did. Failing open, because a check that shuts
  everything down in doubt is worse than no check.
- **Agent credentials can be rotated.** Listing and revoking a machine's
  credentials, an optional end date, and a purpose to tell three apart.
  Issuing still never revokes, so rotation has no gap.
- **Alert severity, suppression, retry and a dead-man beat.** Every rule
  carries a severity, in one table that is also what the gauge iterates. A
  machine inside a declared maintenance window does not page. A refused
  delivery is kept and tried again. Once a minute the leader posts a heartbeat
  — alert on its *absence*, because every rule here fires only when something
  is wrong and a controller that is down produces the same stream a healthy
  cell does.
- **Labels can be set from the console**, and an object's page shows its
  history: what was accepted, by whom, and what was refused, in the words the
  person read.
- **A command-line client**, `velstra`, built from the same description of the
  platform the console and the OpenAPI document are.
- **A load balancer that balances on a cell with no fabric.** Until now a
  `LoadBalancer` was programmed into the Velstra fabric and nowhere else: on the
  local-bridge datapath it took an address, said nothing and forwarded nothing,
  while being in the model, on the console, in the contract and in the API. Each
  node now holds the balancer's address on its own bridge and splices
  connections to the members it carries, reporting what it serves in
  `status.balancers` — from which the controller says `Served`, naming the
  nodes, or `NoDataPlane` when nothing is.

  It **passes TLS through** rather than terminating it: the guest presents the
  certificate and this platform never holds a tenant's private key, which it has
  nowhere safe to keep — it refuses `encryptionKey` for that reason and would be
  contradicting itself. It balances only across members on the node holding the
  address, because without a fabric there is no path to a guest on another
  machine, and it is round robin. All three are said out loud rather than left
  to be discovered.
- **Router advertisements** on a cell whose datapath is the local bridge: one
  prefix per IPv6 segment, on-link and autonomous, plus an answer to a router
  solicitation. It is the only mechanism that configures a guest with no IPv4
  address, because every other route into a guest's configuration goes through
  a metadata service on a v4 link-local. On a fabric cell the node says nothing:
  a router claiming a link it does not route is worse than no router.
- **DNS.** A resolver on every node, authoritative for the cell's guests and
  forwarding for everything else, with its own `velstra.internal` namespace.
- **The package is installed in CI**, on the Ubuntu runner it targets: the
  binaries run, the units are installed and none is enabled, installing a
  second time does what an upgrade does, and removing it takes the binaries
  away. Everything the package check proved before was proved by *inspecting*
  the file.

### Fixed

- **A node never said what carried its guests' traffic.** `status.datapath` now
  reports it — `tap`, `local-network`, `fabric` or `fake` — and a bare `tap`
  raises `node-wires-nowhere`, naming the setting that fixes it. `tap` is the
  default when a seed names no datapath, and it gives a guest a wire with
  nothing at the other end: an address, a gateway that answers nothing, and no
  way off its own machine. Correct on a node whose fabric carries the segment,
  a silent dead end on one without — and nothing anywhere said which a node
  was. Found on a live cell, where it was the whole reason guests could not
  reach the internet.

- **A dual-stack guest's second family had no default route.** The netplan gave
  the default route to the first NIC only — right while every guest had one
  family, and silently wrong the moment one had two: a v4 default and a v6
  default are different route tables and cannot race. A guest with a v6 address
  and no way off its own link could reach its neighbour and nothing else.

- **A volume from an image worked on one storage backend of three.** The node
  files images under their digest; the directory pool looked for the resource
  name with its slashes flattened, LVM treated the name as a path, and Ceph
  needed an image somebody had imported by hand. The pool now resolves the name
  once, from the object's digest, and Ceph brings an image into the cluster
  from the local cache when it has never seen it.
- **An image could not be edited at all.** A patch was judged as if it were a
  whole spec, so every save came back "an image says which bytes it is".
- **A refused guest left a wire behind**: the retirement check ran after the
  default port was minted.
- **A deleted pool kept a working credential for ever.** The branch that
  forgets an agent's token named only nodes.
- **A deleted project's usage readings were kept for ever.** Pruning ran inside
  the project's own reconcile, and a project that no longer exists is never
  reconciled. A sweep now collects the ones whose deletion nobody saw.
- **A guest's root disk did not count against the storage quota** at the door,
  though the project's own status always said it did. The device limit was
  reported and never enforced.
- **The boot test filled the disk.** Its scratch directory was removed only on
  success, so every failing run left two hundred megabytes behind — enough of
  them to fill a fourteen-gigabyte `/tmp`, after which nothing on the machine
  could build.
- **The node's image cache grew for ever.** It now forgets images the cell no
  longer has and nothing here needs, after thirty days; a broken download is
  moved aside so a retry has somewhere to land.

### Security

- **The console stream can be encrypted, and says when it is not.** The API's
  own port has spoken TLS from the start; the hop behind it — API to node —
  went in the clear across whatever network a cell's machines share, carrying a
  serial line including whatever an operator types into it. A node given
  `--console-tls-cert`/`--console-tls-key` now serves it over TLS and reports
  `status.consoleTls`, which is what makes the API connect with `wss://`
  (trusting `--console-ca`). And when it is *not* encrypted the platform says
  so: a column on the Nodes board, a line on the console screen before anybody
  types, and a warning in the API's log on every attach. The silence was the
  problem — a cleartext console looks exactly like an encrypted one. Where the
  certificate comes from is still the operator's: a control-plane machine
  already holds a pair that fits, and nothing in this platform issues one to a
  node that does not.

- Sign-in has a throttle with a growing wait, sessions and refusals are
  audited, and every change carries its author.
