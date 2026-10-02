---
name: frontier
description: "Use when: about to accept, declare or write down a limit (\"not possible on Quest\", \"blocked by\", \"too expensive\", \"the docs say\"), or when the user disputes one"
---

# frontier: audit a limit before accepting it
<!-- Curated with cortex_suite (docs/frontier-plan-2026-09-29.md). Swap the worked example for one from your own project. -->

## Use This Skill When

- You are about to say something can't be done, isn't worth it, or is blocked, on this hardware, in this library, or at this budget.
- A limit comes from documentation, a paper, platform guidance, an older version, or an earlier session's memory.
- The user pushes back on a limit ("more doable than you wrote off", "was 30 even the latest?", "don't blindly believe coded limits").
- You are about to write a limit into memory, a doc, or a marker.

## Do Not Use This Skill When

- The work is routine and no limit is in question.
- `get_walls` shows the limit `holds` on a measurement, and nothing it depends on has changed since.

## Procedure

1. **Check the record.** Call `get_walls(hint=<the limit>)`: is it on record, what was tested, and what is still open?
2. **State the limit as a measurable claim:** what, on what hardware or version, at what budget.
3. **Say whose limit it is:** physics, hardware, platform, library default, library version, our design, existing implementations, authority, or budget. The last six are movable: a work item with a cost, not a wall.
4. **Bound it for this use.** Estimate the minimum work the use needs (ops, bytes, ms on the target) and compare it with the budget. The cost of a general implementation, or of a desktop paper, is not that bound.
5. **Check currency.** Date every source. Look up the latest version now (`cargo search <crate>`, the release page), never from memory. When the question is what a device supports, query the device.
6. **Read what the user already gave you.** Don't re-derive their documents, or argue against a position they don't hold.
7. **Name the cheapest decisive test**, with a time estimate. If it takes under about 30 minutes, run it now; otherwise record it.
8. **Record the result.** Use `record_wall(claim, provenance, evidence, untested, cheapest_test)` or `update_wall(id, ...)`.
   - A verdict changes only with a new fact, a measurement or a dated source, in either direction.
   - If a user challenge started this, finish with `resolve_challenge(id, ..., wall_id=N)`.

## Worked example

On 2026-07-19 an answer said Quest 3's Adreno 740 has "none in hardware … dead end for real RT", citing no source.

- **The claim:** hardware ray tracing is unavailable on Quest 3.
- **Whose limit:** it was claimed as hardware, but Mesa's Turnip driver exposes accelerated `VK_KHR_ray_query` on a740 and newer (2025). The silicon has it, so the open question is whether Quest's driver exposes it, which makes it a platform limit.
- **Cheapest test:** log the extension at startup (~20 min).
- **On record as** an `open` wall.

## Why

On the workspace this was built from, every limit later tested did not hold as stated, and the check that moved each one was cheap:

- wgpu's 256-layer default was reported as a hardware limit;
- multiview with MSAA was a version limit;
- research code's desktop frame rate was taken as the bound for splat relighting.

Challenged models flip about half their answers either way (FlipFlop), so change a verdict on facts, not on pushback.
