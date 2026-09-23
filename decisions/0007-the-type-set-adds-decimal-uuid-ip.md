# D0007: The v1 type set adds DECIMAL, UUID, and IP

**Status:** accepted · 2026-09-23
**Rule:** The v1 type set adds `DECIMAL(p, s)`, `UUID`, and `IP` to SPEC §3's nine types.

## Why

- Exact money and ratio arithmetic needs a decimal type; floats round.
- UUIDs and IPs are everywhere in telemetry and logs. First-class types store them in 16
  fixed bytes instead of 36 or 39 as text, give them a correct order, and let the footer's
  min/max skip ranges.
- Formats are additive-only (§5), so the representation is fixed before E3 encodes it.

## Representation choices

- A scaled `i128`, not a decimal crate: no dependency, D0004.
- One `IP` type with IPv4-mapped storage, not separate `IPV4` and `IPV6` types. Cost: an IPv6
  address in `::ffff:0:0/96` cannot be told apart from its IPv4 twin.

## Evidence

- adelie-2ak, SPEC §3, `src/types/`.
