# Networking

`feos-net` (`libs/net`) is a small TCP/IP stack written for feos, not vendored: Ethernet,
ARP, IPv4, ICMP echo, UDP, a DHCP client, a DNS client and TCP. It's under the workspace's
`MIT OR Apache-2.0`. Nothing in it touches hardware.

```
apps (ifconfig, ping, nslookup, wget, nc)
        |  feos_hal::net::Network, through Platform::net
libs/net   service.rs   blocking socket calls, a lock around one Stack
           stack.rs     addresses, neighbours, sockets; non-blocking; told what time it is
           tcp.rs       one connection's state machine, with no I/O of its own
           dhcp.rs dns.rs wire.rs
        |  feos_hal::net::NetDevice
drivers/net/virtio      virtio-net on either transport
```

`apps/commands/src/netd.rs` is the network task. It takes in what the card received and
sends what's due, sleeping on the card's interrupt or until the next timer, so nothing else
blocks on the network. A task that wants something takes the stack's lock, tries it, and
if it has to wait, sleeps on a word `netd` moves when anything happens. It wakes at least
every 50 ms, which is where a kill or a timeout gets noticed.

A machine without a card has no `Platform::net` and no `netd`, and boots as before.

## Bring-up

The board finds the first virtio-net (`-device virtio-net-device` on AArch64,
`virtio-net-pci` on either), gives it the best interrupt it can (MSI-X, INTx or an MMIO
line, or polls), and hands the kernel a `NetBinding`. DHCP starts as soon as `netd` runs,
and `[net] lease ...` in the log says when it's done.

## Commands

| Command | |
|---|---|
| `ifconfig`, `ip` | Address, lease and counters. `-s` lists sockets. |
| `ping [-c n] [-s size] host` | ICMP echo. The result of a failed `ping` is status 1. |
| `nslookup name` | Through the name server DHCP gave. |
| `wget [-O file] http://host[:port]/path` | HTTP/1.0 GET, saved to the VFS, checked against `Content-Length`. |
| `nc [-s text] [-f file] [-o file] host port` | Sends what it's given (`-s`, `-f`, or else standard input), closes its side and takes everything back: to a file with `-o`, or stdout. |
| `nc -l [-k] [-s text] [-f file] [-o file] port` | The same, for one connection, or for each with `-k`. |

Relative paths are from `/`: external commands have no current directory.

## TCP

Retransmission on an RFC 6298 timer, with go-back-N after a timeout, and fast retransmit on
three duplicate ACKs. Slow start and congestion avoidance (RFC 5681). Out-of-order
reassembly, with a bound on what's held. Flow control with zero-window probes. All the
close states, with TIME-WAIT of 30 s, which gives way when sockets run out. RFC 5961's
challenge ACK for a SYN or a reset that doesn't sit exactly at the next sequence number.

## What's tested, and how

Host tests run with `make test-net`:

- Every parser takes random and corrupted bytes, hundreds of thousands of rounds, and
  mustn't panic. The stack as a whole takes 100,000 malformed frames, then still answers.
- `tcp.rs` runs two connections against a simulated wire with loss (up to 25%), reordering,
  duplication and all three at once, and compares every byte. Also sequence wrap-around,
  shut windows and lost window updates, silent peers, and each close state.

The boot test (`FEOS_BOOT_TEST_ONLY=net`) runs a real guest against QEMU's slirp, with a
Python peer on the far side: a DHCP lease, ping to 10.0.2.2, DNS, TCP out through a
`guestfwd` and to the host address, TCP in through a `hostfwd`, and 8 MiB each way with a
SHA-256 compared at both ends. It repeats on each transport: MMIO, PCI with MSI-X, behind a
bridge, INTx, and INTx on a GICv3.

**DNS offline.** Slirp forwards 10.0.2.3 to the name server in the resolv.conf of the
machine QEMU runs on. Run offline, in a container with `--network none` and resolv.conf
pointing at 127.0.0.1, that's a little server in the test, which is the only way the test
tries DNS. Anywhere else, it says it didn't. `make docker-boot-test` sets this up.

Slirp is reliable, so nothing retransmits in a boot test: `0 segments retransmitted` is what
it reports. Loss and reordering are only tested on the host.

## Limits

- IPv4 only, and one interface.
- No IP fragmentation or reassembly: a fragment is dropped, and this host sets Don't Fragment.
- No window scaling, so a connection's window tops out at 64 KiB. No SACK, timestamps,
  delayed ACKs or Nagle.
- ICMP is echo only: nothing is told about an unreachable host until its timer runs out.
- Sequence numbers, ports and DNS IDs come from an xorshift generator seeded with the clock
  and the MAC, which someone who can watch the traffic could predict. There's no source of
  real randomness in the kernel. Don't rely on this stack against an attacker on the path.
- A DNS answer's address isn't checked for being one that makes sense. ARP believes what
  it's told, in answer or not.
- `wget` has no HTTPS, redirects or chunked encoding, none of which HTTP/1.0 asks for.
- Nothing shows the network on the desktop.
