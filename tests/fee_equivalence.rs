// From the independent re-audit (plan 010): `fee_for` must equal Token-2022's `TransferFee::calculate_fee` for every
// input, and be monotone in (basis points, maximum), so lowering either can never trigger FeeRaised.
//   cd programs/airdrop && cargo test --offline --test fee_equivalence
use runners_airdrop::fee_for;
use spl_token_2022::extension::transfer_fee::TransferFee;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 29)
    }
}

fn reference(amount: u64, basis_points: u16, maximum: u64) -> Option<u64> {
    TransferFee { epoch: 0.into(), maximum_fee: maximum.into(), transfer_fee_basis_points: basis_points.into() }.calculate_fee(amount)
}

#[test]
fn fee_for_equals_token_2022_calculate_fee_on_edges_and_random_inputs() {
    let amounts = [0u64, 1, 2, 50, 99, 100, 101, 199, 200, 9_999, 10_000, 10_001, u32::MAX as u64, 1_000_000_000_000_000, u64::MAX - 1, u64::MAX];
    let bps = [0u16, 1, 2, 50, 99, 100, 101, 250, 9_999, 10_000];
    let maxima = [0u64, 1, 2, 5, 100, 10_000_000_000, u64::MAX - 1, u64::MAX];
    for &a in &amounts {
        for &b in &bps {
            for &m in &maxima {
                assert_eq!(fee_for(a, b, m), reference(a, b, m), "amount {a} bps {b} max {m}");
                assert!(fee_for(a, b, m).unwrap() <= a, "fee never exceeds the amount");
            }
        }
    }
    let mut rng = Lcg(0x5eed_1234_abcd_ef01);
    for _ in 0..3_000_000 {
        let a = match rng.next() % 3 { 0 => rng.next(), 1 => rng.next() % 1_000_000_000_000_000, _ => rng.next() % 100_000 };
        let b = (rng.next() % 10_001) as u16;
        let m = match rng.next() % 3 { 0 => u64::MAX, 1 => rng.next(), _ => rng.next() % 1_000_000 };
        assert_eq!(fee_for(a, b, m), reference(a, b, m), "amount {a} bps {b} max {m}");
    }
}

#[test]
fn fee_for_is_monotone_in_basis_points_and_maximum() {
    let mut rng = Lcg(0x0bad_cafe_f00d_0001);
    for _ in 0..1_000_000 {
        let a = if rng.next() % 2 == 0 { rng.next() } else { rng.next() % 10_000_000_000_000 };
        let (b1, b2) = { let x = (rng.next() % 10_001) as u16; let y = (rng.next() % 10_001) as u16; (x.min(y), x.max(y)) };
        let (m1, m2) = { let x = rng.next(); let y = if rng.next() % 2 == 0 { rng.next() } else { u64::MAX }; (x.min(y), x.max(y)) };
        // Lower (or equal) basis points and maximum never yield a higher fee: lowering the fee never triggers FeeRaised.
        assert!(fee_for(a, b1, m1).unwrap() <= fee_for(a, b2, m2).unwrap(), "amount {a} ({b1},{m1}) vs ({b2},{m2})");
        assert!(fee_for(a, b1, m2).unwrap() <= fee_for(a, b2, m2).unwrap());
        assert!(fee_for(a, b2, m1).unwrap() <= fee_for(a, b2, m2).unwrap());
    }
    // Strictly higher basis points with an uncapped maximum raise the fee for any amount of at least 10,000 units,
    // so FeeRaised fires for every real entitlement when the fee goes from 100 to 101 bps.
    for a in [10_000u64, 123_456, 1_000_000_000_000, u64::MAX] {
        assert!(fee_for(a, 101, u64::MAX).unwrap() > fee_for(a, 100, u64::MAX).unwrap());
    }
}
