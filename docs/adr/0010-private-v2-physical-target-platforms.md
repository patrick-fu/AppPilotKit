# Private v2 Broker IPC and physical Target platforms

Physical iOS/Android Targets cannot be expressed on frozen private v1 (`platform = 0/1` only, meaning Simulator/Emulator). Silently extending v1 or encoding a device as platform `0` would either break old peers or collide Simulator meaning. Controller Batch 2026-09-12 accepted a dual-stack private v2: new IPC/bootstrap descriptors carry version `2` and platform `2`/`3`; v1 codecs and `0`/`1` meanings stay byte-identical; only explicit physical Prepare emits v2; open/exchange/close stay v1; replies echo the request version. Noise NK/NNpsk0 prologue versions stay as v1; the v2 launch descriptor is the physical bind. Public CLI/Protocol/SDK, TTLs, and deadlines are unchanged.

## Considered Options

- Extend v1 `platform = 0/1/2/3` — old Brokers `Malformed`; rejected as a silent v1 mutation.
- Reuse bootstrap/IPC platform `0` as an iOS bind family — rejected; it reinterprets Simulator.
- Negotiation/hello operation — extra op, breaks v1 `operation > 4`; unused because every packet already has key `0`.
