//! Pure bandwidth allocator: splits a total kbit/s budget across
//! destinations proportionally by `width*height*fps` weight, honoring
//! per-destination floor/ceiling clamps.
//!
//! This module is intentionally decoupled from the real multi-room data
//! model (`DestinationId`, `Destination`, ...), which doesn't exist as real
//! code yet — it takes plain data in ([`AllocInput`]) and returns plain data
//! out ([`Allocation`]). A later integration stage is expected to build
//! those types from live `Destination` state and translate the result back
//! into per-branch encoder bitrate writes.
//!
//! `Why:` [`allocate`] is a pure function — no I/O, no wall-clock, no RNG —
//! by design, mirroring this codebase's existing determinism/testability
//! stance (see `decode.rs`'s `PacingState`, which takes an explicit `Instant`
//! rather than calling `Instant::now()` itself). A live encoder needs
//! hysteresis/rate-limiting on top of this (see the plan doc's
//! "Live bitrate changes apply with hysteresis... and a rate limit" note) —
//! that's explicitly a later integration concern layered *outside* this
//! function, not something `allocate` itself should do, since folding a
//! timer into the allocator would break the purity this module exists to
//! guarantee.
//!
//! # Algorithm
//!
//! 1. `budget = total_kbps - audio_reserved_kbps` (off the top, saturating).
//! 2. If `Σ floor_kbps > budget`, every destination is starved down to
//!    exactly its floor (never below), and the overshoot is reported rather
//!    than silently exceeding the user's configured maximum — see
//!    [`AllocNote::FloorOvershoot`].
//! 3. Otherwise, `budget` is split proportionally by weight
//!    (`width * height * fps`, computed in `u64` — realistic multi-stream
//!    weights and their sums overflow `u32`) using largest-remainder
//!    rounding, so the parts sum to exactly `budget` with no
//!    floating-point drift.
//! 4. Each destination is clamped into `[floor_kbps, ceiling_kbps]`; any
//!    clamped delta is redistributed proportionally among the
//!    still-unclamped destinations, iterating to a fixed point. This
//!    terminates because each iteration freezes at least one more
//!    destination at a bound, so the unclamped set strictly shrinks. Floor
//!    violations are resolved before ceiling violations within a round: a
//!    floor clamp changes how much pool the rest of the active set has to
//!    split, which can turn an apparent ceiling violation (computed
//!    against the pre-floor-clamp pool) into a share that fits once the
//!    floor destinations' guaranteed cut is removed — resolving floors
//!    first avoids ceiling-clamping a destination that never needed it
//!    (and avoids an unsound pool subtraction).
//! 5. If every destination ends up bound (floor or ceiling) with budget
//!    left over, the leftover is reported as
//!    [`AllocNote::SurplusUnassigned`] rather than silently dropped.

use std::collections::HashMap;

/// Opaque destination identifier. A thin newtype standing in for the real
/// `DestinationId` from the (not-yet-built) multi-room data model — this
/// module must not depend on that model, so it owns its own minimal ID type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DestId(pub u32);

/// Per-destination allocation input: configured (not measured) video
/// parameters plus the bandwidth floor/ceiling for that destination.
///
/// `Why: configured, not measured` — using live measured bitrate as an
/// allocator input would create a feedback loop (allocator reacts to the
/// encoder, encoder reacts to the allocator); using the configured
/// resolution/fps instead keeps `allocate` a pure function of stable
/// inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestSpec {
    pub id: DestId,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub floor_kbps: u32,
    pub ceiling_kbps: Option<u32>,
}

/// Input to [`allocate`]: the total per-client bandwidth budget, an
/// off-the-top reservation for the always-on idle-audio baseline (see the
/// plan doc's always-on silent-baseline `audiomixer` design), and the
/// destinations competing for the remainder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocInput {
    pub total_kbps: u32,
    pub audio_reserved_kbps: u32,
    pub destinations: Vec<DestSpec>,
}

/// Notable events during allocation, surfaced so a caller (eventually the
/// Settings UI's bandwidth-overshoot warning banner) can explain *why* a
/// destination got what it got without re-deriving the reasoning itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocNote {
    /// `Σ floor_kbps + audio_reserved_kbps` exceeded `total_kbps`: every
    /// destination was still given its full floor (never starved), pushing
    /// the assigned total `over_kbps` past the user's configured maximum.
    FloorOvershoot { over_kbps: u32 },
    /// This destination's proportional share was below its floor; it was
    /// raised to exactly `floor_kbps` instead.
    ClampedToFloor(DestId),
    /// This destination's proportional share was above its ceiling; it was
    /// lowered to exactly `ceiling_kbps` instead.
    ClampedToCeiling(DestId),
    /// After all clamping settled, `kbps` of budget had no destination left
    /// to absorb it (e.g. every destination is ceiling-bound with room to
    /// spare) — not silently dropped, just unassigned.
    SurplusUnassigned { kbps: u32 },
}

/// Result of [`allocate`]: a per-destination kbit/s assignment plus enough
/// bookkeeping to drive a UI warning banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    /// One entry per input destination, in the same order as
    /// `AllocInput::destinations`.
    pub per_destination: Vec<(DestId, u32)>,
    /// `Σ per_destination + audio_reserved_kbps` — what was actually
    /// committed, which may exceed `total_kbps` only in the
    /// [`AllocNote::FloorOvershoot`] case.
    pub assigned_total_kbps: u32,
    /// How far `assigned_total_kbps` exceeds `total_kbps`; `0` unless
    /// floors (plus the audio reservation) alone don't fit the budget.
    pub overshoot_kbps: u32,
    pub notes: Vec<AllocNote>,
}

/// Split `total_kbps` (minus `audio_reserved_kbps`) across `destinations`
/// proportionally by `width * height * fps`, honoring each destination's
/// `[floor_kbps, ceiling_kbps]` bound. Pure, deterministic, no I/O.
pub fn allocate(input: &AllocInput) -> Allocation {
    let destinations = &input.destinations;

    if destinations.is_empty() {
        let assigned_total_kbps = input.audio_reserved_kbps;
        let overshoot_kbps = assigned_total_kbps.saturating_sub(input.total_kbps);
        return Allocation {
            per_destination: Vec::new(),
            assigned_total_kbps,
            overshoot_kbps,
            notes: Vec::new(),
        };
    }

    let budget = input.total_kbps.saturating_sub(input.audio_reserved_kbps) as u64;
    let floor_sum: u64 = destinations.iter().map(|d| d.floor_kbps as u64).sum();

    if floor_sum > budget {
        // Floor overshoot: nobody gets starved below floor, ever — every
        // destination gets exactly its floor and we report by how much
        // that pushed the assigned total past the user's configured max.
        let per_destination: Vec<(DestId, u32)> =
            destinations.iter().map(|d| (d.id, d.floor_kbps)).collect();
        let assigned_total_u64 = floor_sum + input.audio_reserved_kbps as u64;
        let assigned_total_kbps = saturating_to_u32(assigned_total_u64);
        let overshoot_kbps = assigned_total_kbps.saturating_sub(input.total_kbps);
        return Allocation {
            per_destination,
            assigned_total_kbps,
            overshoot_kbps,
            notes: vec![AllocNote::FloorOvershoot {
                over_kbps: overshoot_kbps,
            }],
        };
    }

    // Proportional split by weight, clamped to [floor, ceiling], with the
    // clamped delta redistributed among the still-unclamped set until a
    // fixed point: each iteration below either freezes >=1 more
    // destination at a bound (shrinking `active`) or finishes cleanly, so
    // this always terminates.
    //
    // `Why: floor violations are clamped before ceiling violations, never
    // in the same round` — clamping a destination up to its floor *steals*
    // budget from the pool the rest of `active` is competing for, which can
    // retroactively turn what looked like a ceiling violation (computed
    // against the pre-floor-clamp pool) into a share that fits comfortably
    // once the floor destinations' guaranteed cut is removed. Resolving
    // floor violators first and re-deriving shares against the shrunk pool
    // avoids ceiling-clamping a destination that never actually needed it.
    // It also keeps the bookkeeping sound: `floor_sum(active) <= pool` is
    // an invariant maintained by the initial guard above and preserved by
    // every floor-only round (subtracting a subset's floors from a pool
    // that already covers the whole active set's floors can't underflow),
    // and a ceiling-only round can only ever consume less than the shares
    // it replaces (`ceiling < share` by definition), so it can't underflow
    // either. Clamping both kinds in the same round using stale shares can
    // (and did, pre-fix) drive `pool` negative even though the situation
    // is feasible.
    let mut final_values: HashMap<DestId, u64> = HashMap::with_capacity(destinations.len());
    let mut notes: Vec<AllocNote> = Vec::new();
    let mut active: Vec<&DestSpec> = destinations.iter().collect();
    let mut pool: u64 = budget;

    loop {
        if active.is_empty() {
            break;
        }

        let weighted: Vec<(DestId, u64)> = active.iter().map(|d| (d.id, weight(d))).collect();
        let shares = largest_remainder_split(pool, &weighted);
        let share_of = |id: DestId| -> u64 {
            *shares
                .get(&id)
                .expect("share computed for every active destination")
        };

        let any_floor_violation = active.iter().any(|d| share_of(d.id) < d.floor_kbps as u64);

        let mut next_active = Vec::with_capacity(active.len());
        let mut consumed: u64 = 0;
        let mut any_clamped = false;

        for d in &active {
            let share = share_of(d.id);
            let floor = d.floor_kbps as u64;
            let ceiling = d.ceiling_kbps.map(|c| c as u64);

            if share < floor {
                final_values.insert(d.id, floor);
                notes.push(AllocNote::ClampedToFloor(d.id));
                consumed += floor;
                any_clamped = true;
            } else if !any_floor_violation && ceiling.is_some_and(|c| share > c) {
                // Only clamp ceiling violators once this round has no
                // floor violators left to resolve first (see `Why:` above).
                let ceiling = ceiling.expect("checked Some above");
                final_values.insert(d.id, ceiling);
                notes.push(AllocNote::ClampedToCeiling(d.id));
                consumed += ceiling;
                any_clamped = true;
            } else {
                next_active.push(*d);
            }
        }

        pool -= consumed;

        if !any_clamped {
            // Nobody was clamped this round: the computed shares stand as
            // final for everyone still active, and they sum to exactly
            // `pool` by construction (largest-remainder rounding).
            for d in &active {
                final_values.insert(d.id, share_of(d.id));
            }
            pool = pool.saturating_sub(shares.values().sum());
            break;
        }

        active = next_active;
    }

    if pool > 0 {
        notes.push(AllocNote::SurplusUnassigned {
            kbps: saturating_to_u32(pool),
        });
    }

    let per_destination: Vec<(DestId, u32)> = destinations
        .iter()
        .map(|d| {
            let v = *final_values
                .get(&d.id)
                .expect("every destination gets a final value");
            (d.id, saturating_to_u32(v))
        })
        .collect();

    let assigned_video: u64 = final_values.values().sum();
    let assigned_total_kbps = saturating_to_u32(assigned_video + input.audio_reserved_kbps as u64);
    let overshoot_kbps = assigned_total_kbps.saturating_sub(input.total_kbps);

    Allocation {
        per_destination,
        assigned_total_kbps,
        overshoot_kbps,
        notes,
    }
}

/// `width * height * fps` as `u64` — per the plan doc, `u32` overflows at
/// realistic multi-stream totals (a single 4K@240 destination alone is
/// already ~2 billion; summing a handful overflows `u32`'s ~4.29 billion
/// ceiling).
fn weight(d: &DestSpec) -> u64 {
    (d.width as u64) * (d.height as u64) * (d.fps as u64)
}

/// Split `pool` across `weighted` proportionally to each entry's weight,
/// using largest-remainder rounding so the parts sum to exactly `pool`. If
/// every weight is zero, splits evenly instead (largest-remainder over
/// equal weights). Ties (equal remainders, or the all-zero-weight case) are
/// broken deterministically by ascending [`DestId`], so results are
/// reproducible.
///
/// `Why: u128` intermediate — `pool * weight` can exceed `u64` for
/// extreme-but-representable inputs (e.g. an 8K@240 weight around `1e10`
/// times a near-`u32::MAX` pool), so the multiplication is done in `u128`
/// before dividing back down; the divided result always fits back in `u64`
/// since it's bounded by `pool` itself.
fn largest_remainder_split(pool: u64, weighted: &[(DestId, u64)]) -> HashMap<DestId, u64> {
    let mut result: HashMap<DestId, u64> = HashMap::with_capacity(weighted.len());
    if weighted.is_empty() {
        return result;
    }

    let total_weight: u64 = weighted.iter().map(|(_, w)| w).sum();

    if total_weight == 0 {
        let mut ids: Vec<DestId> = weighted.iter().map(|(id, _)| *id).collect();
        ids.sort();
        let n = ids.len() as u64;
        let base = pool / n;
        let mut remainder = pool % n;
        for id in ids {
            let extra = if remainder > 0 {
                remainder -= 1;
                1
            } else {
                0
            };
            result.insert(id, base + extra);
        }
        return result;
    }

    let mut assigned_sum: u64 = 0;
    let mut remainders: Vec<(DestId, u128)> = Vec::with_capacity(weighted.len());
    for (id, w) in weighted {
        let num = pool as u128 * *w as u128;
        let base = (num / total_weight as u128) as u64;
        let rem = num % total_weight as u128;
        result.insert(*id, base);
        assigned_sum += base;
        remainders.push((*id, rem));
    }

    let mut leftover = pool.saturating_sub(assigned_sum);
    remainders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (id, _) in remainders {
        if leftover == 0 {
            break;
        }
        *result.get_mut(&id).expect("every id has a base share") += 1;
        leftover -= 1;
    }

    result
}

fn saturating_to_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(
        id: u32,
        width: u32,
        height: u32,
        fps: u32,
        floor_kbps: u32,
        ceiling_kbps: Option<u32>,
    ) -> DestSpec {
        DestSpec {
            id: DestId(id),
            width,
            height,
            fps,
            floor_kbps,
            ceiling_kbps,
        }
    }

    fn find(alloc: &Allocation, id: u32) -> u32 {
        alloc
            .per_destination
            .iter()
            .find(|(d, _)| *d == DestId(id))
            .unwrap_or_else(|| panic!("destination {id} missing from allocation"))
            .1
    }

    /// The plan doc's worked example: 30,000 kbit/s total, one destination
    /// at 1920x1080@240 and one at 1920x1080@30, no floors/ceilings — the
    /// ~88.9%/11.1% weight split should land within a tight tolerance of
    /// 26,666.7 / 3,333.3 kbit/s.
    #[test]
    fn worked_example_1080p240_vs_1080p30() {
        let input = AllocInput {
            total_kbps: 30_000,
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 1920, 1080, 240, 0, None),
                dest(2, 1920, 1080, 30, 0, None),
            ],
        };
        let alloc = allocate(&input);
        let a = find(&alloc, 1) as f64;
        let b = find(&alloc, 2) as f64;
        assert!((a - 26_666.7).abs() < 1.0, "expected ~26666.7, got {a}");
        assert!((b - 3_333.3).abs() < 1.0, "expected ~3333.3, got {b}");
        assert_eq!(alloc.assigned_total_kbps, 30_000);
        assert_eq!(alloc.overshoot_kbps, 0);
    }

    /// Assigned shares must sum to exactly `budget` when floors/ceilings
    /// don't force an overshoot or leave surplus — no rounding drift.
    #[test]
    fn shares_sum_exactly_to_budget() {
        let input = AllocInput {
            total_kbps: 10_000,
            audio_reserved_kbps: 500,
            destinations: vec![
                dest(1, 1280, 720, 30, 0, None),
                dest(2, 1920, 1080, 60, 0, None),
                dest(3, 640, 480, 15, 0, None),
            ],
        };
        let alloc = allocate(&input);
        let sum: u32 = alloc.per_destination.iter().map(|(_, kbps)| *kbps).sum();
        assert_eq!(sum, 10_000 - 500);
        assert_eq!(alloc.assigned_total_kbps, 10_000);
        assert_eq!(alloc.overshoot_kbps, 0);
        assert!(alloc.notes.is_empty());
    }

    /// Floor overshoot: every destination still gets its full floor, and
    /// the reported total/overshoot reflect exactly how far that pushed
    /// past `total_kbps`.
    #[test]
    fn floor_overshoot_sums_to_floor_and_reports_exact_overshoot() {
        let input = AllocInput {
            total_kbps: 1_000,
            audio_reserved_kbps: 100,
            destinations: vec![
                dest(1, 1920, 1080, 30, 600, None),
                dest(2, 1920, 1080, 30, 600, None),
            ],
        };
        let alloc = allocate(&input);
        assert_eq!(find(&alloc, 1), 600);
        assert_eq!(find(&alloc, 2), 600);
        // assigned_total = floor_sum + audio = 1200 + 100 = 1300
        assert_eq!(alloc.assigned_total_kbps, 1_300);
        // overshoot = assigned_total - total_kbps = 1300 - 1000 = 300
        assert_eq!(alloc.overshoot_kbps, 300);
        assert!(alloc
            .notes
            .iter()
            .any(|n| matches!(n, AllocNote::FloorOvershoot { over_kbps: 300 })));
    }

    /// `overshoot_kbps > 0` iff `Σfloor + audio_reserved > total_kbps`,
    /// across floor-overshoot, comfortably-fitting, and audio-alone-exceeds
    /// scenarios.
    #[test]
    fn overshoot_iff_floor_plus_audio_exceeds_total() {
        let cases: Vec<AllocInput> = vec![
            // Fits comfortably: no overshoot.
            AllocInput {
                total_kbps: 5_000,
                audio_reserved_kbps: 100,
                destinations: vec![dest(1, 1280, 720, 30, 500, None)],
            },
            // Floors alone overshoot.
            AllocInput {
                total_kbps: 500,
                audio_reserved_kbps: 0,
                destinations: vec![dest(1, 1280, 720, 30, 800, None)],
            },
            // Audio alone exceeds total, floors are zero (still goes
            // through the normal path, not the floor-overshoot branch).
            AllocInput {
                total_kbps: 50,
                audio_reserved_kbps: 200,
                destinations: vec![dest(1, 1280, 720, 30, 0, None)],
            },
            // Exactly on the boundary: floor + audio == total => no overshoot.
            AllocInput {
                total_kbps: 1_000,
                audio_reserved_kbps: 200,
                destinations: vec![dest(1, 1280, 720, 30, 800, None)],
            },
        ];

        for input in &cases {
            let alloc = allocate(input);
            let floor_sum: u64 = input.destinations.iter().map(|d| d.floor_kbps as u64).sum();
            let expects_overshoot =
                floor_sum + input.audio_reserved_kbps as u64 > input.total_kbps as u64;
            assert_eq!(
                alloc.overshoot_kbps > 0,
                expects_overshoot,
                "input {input:?} => alloc {alloc:?}"
            );
        }
    }

    /// Every destination stays within its [floor, ceiling] bound across a
    /// handful of generated scenarios, including ones where ceilings force
    /// redistribution across multiple iterations.
    #[test]
    fn never_below_floor_never_above_ceiling() {
        let scenarios = vec![
            AllocInput {
                total_kbps: 20_000,
                audio_reserved_kbps: 200,
                destinations: vec![
                    dest(1, 3840, 2160, 60, 1_000, Some(3_000)),
                    dest(2, 1920, 1080, 30, 500, None),
                    dest(3, 640, 480, 15, 100, Some(200)),
                ],
            },
            AllocInput {
                total_kbps: 3_000,
                audio_reserved_kbps: 0,
                destinations: vec![
                    dest(1, 1920, 1080, 60, 2_000, Some(2_100)),
                    dest(2, 1920, 1080, 60, 200, Some(200)),
                    dest(3, 1920, 1080, 60, 0, Some(50)),
                ],
            },
            AllocInput {
                total_kbps: 8_000,
                audio_reserved_kbps: 300,
                destinations: vec![
                    dest(1, 1920, 1080, 240, 0, Some(1_000)),
                    dest(2, 1920, 1080, 30, 0, Some(1_000)),
                    dest(3, 1280, 720, 30, 0, Some(1_000)),
                ],
            },
        ];

        for input in &scenarios {
            let alloc = allocate(input);
            for d in &input.destinations {
                let assigned = find(&alloc, d.id.0);
                assert!(
                    assigned >= d.floor_kbps,
                    "input {input:?}: {assigned} below floor for {d:?}"
                );
                if let Some(ceiling) = d.ceiling_kbps {
                    assert!(
                        assigned <= ceiling,
                        "input {input:?}: {assigned} above ceiling for {d:?}"
                    );
                }
            }
        }
    }

    /// If every destination is ceiling-bound with room to spare, the
    /// leftover is reported as unassigned surplus rather than dropped.
    #[test]
    fn surplus_reported_when_all_destinations_hit_ceiling() {
        let input = AllocInput {
            total_kbps: 10_000,
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 1920, 1080, 60, 0, Some(1_000)),
                dest(2, 1280, 720, 30, 0, Some(500)),
            ],
        };
        let alloc = allocate(&input);
        assert_eq!(find(&alloc, 1), 1_000);
        assert_eq!(find(&alloc, 2), 500);
        assert!(alloc
            .notes
            .iter()
            .any(|n| matches!(n, AllocNote::SurplusUnassigned { kbps: 8_500 })));
        assert!(alloc
            .notes
            .iter()
            .any(|n| matches!(n, AllocNote::ClampedToCeiling(DestId(1)))));
        assert!(alloc
            .notes
            .iter()
            .any(|n| matches!(n, AllocNote::ClampedToCeiling(DestId(2)))));
    }

    /// Raising one destination's fps (weight) must never *raise* another
    /// destination's assigned share, all else equal.
    #[test]
    fn monotonicity_raising_one_weight_never_raises_others_share() {
        let baseline = AllocInput {
            total_kbps: 15_000,
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 1920, 1080, 30, 0, None),
                dest(2, 1920, 1080, 30, 0, None),
            ],
        };
        let raised = AllocInput {
            total_kbps: 15_000,
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 1920, 1080, 120, 0, None),
                dest(2, 1920, 1080, 30, 0, None),
            ],
        };

        let alloc_baseline = allocate(&baseline);
        let alloc_raised = allocate(&raised);

        let dest2_baseline = find(&alloc_baseline, 2);
        let dest2_raised = find(&alloc_raised, 2);
        assert!(
            dest2_raised <= dest2_baseline,
            "dest 2's share rose ({dest2_baseline} -> {dest2_raised}) after dest 1's weight was raised"
        );
        // And dest 1's own share should have risen (sanity check the setup
        // actually exercises the intended direction).
        assert!(find(&alloc_raised, 1) >= find(&alloc_baseline, 1));
    }

    /// Empty destination list: no panic, empty allocation.
    #[test]
    fn empty_destinations_returns_empty_allocation() {
        let input = AllocInput {
            total_kbps: 5_000,
            audio_reserved_kbps: 100,
            destinations: vec![],
        };
        let alloc = allocate(&input);
        assert!(alloc.per_destination.is_empty());
        assert_eq!(alloc.assigned_total_kbps, 100);
        assert_eq!(alloc.overshoot_kbps, 0);
        assert!(alloc.notes.is_empty());
    }

    /// `total_kbps == 0`: everything without a floor gets zero, no panic.
    #[test]
    fn zero_total_kbps_with_no_floors() {
        let input = AllocInput {
            total_kbps: 0,
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 1920, 1080, 30, 0, None),
                dest(2, 1280, 720, 30, 0, None),
            ],
        };
        let alloc = allocate(&input);
        assert_eq!(find(&alloc, 1), 0);
        assert_eq!(find(&alloc, 2), 0);
        assert_eq!(alloc.assigned_total_kbps, 0);
        assert_eq!(alloc.overshoot_kbps, 0);
    }

    /// `total_kbps == 0` with a nonzero floor: floor still wins, overshoot
    /// reported.
    #[test]
    fn zero_total_kbps_with_floor_overshoots() {
        let input = AllocInput {
            total_kbps: 0,
            audio_reserved_kbps: 0,
            destinations: vec![dest(1, 1920, 1080, 30, 100, None)],
        };
        let alloc = allocate(&input);
        assert_eq!(find(&alloc, 1), 100);
        assert_eq!(alloc.assigned_total_kbps, 100);
        assert_eq!(alloc.overshoot_kbps, 100);
    }

    /// Single destination: gets the whole budget (clamped to its bounds).
    #[test]
    fn single_destination_gets_whole_budget() {
        let input = AllocInput {
            total_kbps: 4_000,
            audio_reserved_kbps: 200,
            destinations: vec![dest(1, 1920, 1080, 30, 0, None)],
        };
        let alloc = allocate(&input);
        assert_eq!(find(&alloc, 1), 3_800);
        assert_eq!(alloc.assigned_total_kbps, 4_000);
        assert_eq!(alloc.overshoot_kbps, 0);
    }

    /// Identical weights (a tie): the split is deterministic and
    /// reproducible (broken by ascending `DestId`) rather than depending on
    /// hash-map iteration order.
    #[test]
    fn identical_weights_tie_break_is_deterministic() {
        let input = AllocInput {
            total_kbps: 101, // odd, so one destination gets the extra 1 kbit/s
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 1920, 1080, 30, 0, None),
                dest(2, 1920, 1080, 30, 0, None),
            ],
        };
        let first = allocate(&input);
        for _ in 0..20 {
            let again = allocate(&input);
            assert_eq!(
                first.per_destination, again.per_destination,
                "tie-break must be deterministic"
            );
        }
        let sum: u32 = first.per_destination.iter().map(|(_, kbps)| *kbps).sum();
        assert_eq!(sum, 101);
        // Lower DestId wins the odd kbit/s by the documented tie-break rule.
        assert_eq!(find(&first, 1), 51);
        assert_eq!(find(&first, 2), 50);
    }

    /// Regression: a floor-violator and a ceiling-violator in the same
    /// round, where naively clamping both against their *pre-clamp* shares
    /// (`floor(80) + ceiling(30) = 110 > pool(100)`) previously underflowed
    /// `pool` (`attempt to subtract with overflow`, panicking in debug
    /// builds / silently corrupting the allocation in release). The
    /// situation is actually feasible — d1 takes its floor of 80, leaving
    /// exactly 20 for d2, which is under d2's ceiling of 30 — so once d1's
    /// floor is resolved first, d2 never needs to be ceiling-clamped at
    /// all.
    #[test]
    fn floor_clamp_can_rescue_a_stale_ceiling_violation() {
        let input = AllocInput {
            total_kbps: 100,
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 100, 100, 1, 80, None),
                dest(2, 200, 200, 1, 0, Some(30)),
            ],
        };
        let alloc = allocate(&input);
        assert_eq!(find(&alloc, 1), 80);
        assert_eq!(find(&alloc, 2), 20);
        assert_eq!(alloc.assigned_total_kbps, 100);
        assert_eq!(alloc.overshoot_kbps, 0);
        assert!(alloc.notes.iter().any(|n| matches!(n, AllocNote::ClampedToFloor(DestId(1)))));
        // d2 is never ceiling-clamped: its true remaining share (20) is
        // within its ceiling (30) once d1's floor claim is accounted for.
        assert!(!alloc
            .notes
            .iter()
            .any(|n| matches!(n, AllocNote::ClampedToCeiling(DestId(2)))));
    }

    /// All ceilings below what proportional split would assign: every
    /// destination is ceiling-clamped and the rest becomes reported
    /// surplus, never silently dropped.
    #[test]
    fn all_ceilings_below_proportional_share() {
        let input = AllocInput {
            total_kbps: 100_000,
            audio_reserved_kbps: 0,
            destinations: vec![
                dest(1, 1920, 1080, 240, 10, Some(50)),
                dest(2, 1920, 1080, 30, 10, Some(20)),
            ],
        };
        let alloc = allocate(&input);
        assert_eq!(find(&alloc, 1), 50);
        assert_eq!(find(&alloc, 2), 20);
        let surplus_kbps = 100_000 - 70;
        assert!(alloc
            .notes
            .iter()
            .any(|n| matches!(n, AllocNote::SurplusUnassigned { kbps } if *kbps == surplus_kbps)));
    }

    /// A generous multi-4K/8K@240 fan-out (well beyond realistic
    /// deployments) at a near-`u32::MAX` budget must not overflow or panic
    /// in the weight (`u64`) or largest-remainder (`u128` intermediate)
    /// math, and shares must still sum exactly to the budget.
    #[test]
    fn many_4k_and_8k_destinations_no_overflow() {
        let mut destinations = Vec::new();
        for i in 0..64u32 {
            let (width, height) = if i % 2 == 0 { (3840, 2160) } else { (7680, 4320) };
            destinations.push(dest(i, width, height, 240, 0, None));
        }
        let input = AllocInput {
            total_kbps: u32::MAX,
            audio_reserved_kbps: 0,
            destinations,
        };
        let alloc = allocate(&input);
        let sum: u64 = alloc.per_destination.iter().map(|(_, k)| *k as u64).sum();
        assert_eq!(sum, u32::MAX as u64);
    }
}
