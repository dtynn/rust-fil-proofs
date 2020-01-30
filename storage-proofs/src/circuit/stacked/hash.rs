use bellperson::gadgets::{
    boolean::Boolean,
    sha256::sha256 as sha256_circuit,
    {multipack, num},
};
use bellperson::{ConstraintSystem, SynthesisError};
use ff::PrimeField;
use fil_sapling_crypto::jubjub::JubjubEngine;

/// Hash a list of bits.
pub fn hash_single_column<E, CS>(
    mut cs: CS,
    _params: &E::Params,
    rows: &[Option<E::Fr>],
) -> Result<num::AllocatedNum<E>, SynthesisError>
where
    E: JubjubEngine,
    CS: ConstraintSystem<E>,
{
    let mut bits = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let row_num = num::AllocatedNum::alloc(
            cs.namespace(|| format!("hash_single_column_row_{}_num", i)),
            || {
                row.map(Into::into)
                    .ok_or_else(|| SynthesisError::AssignmentMissing)
            },
        )?;

        let mut row_bits =
            row_num.to_bits_le(cs.namespace(|| format!("hash_single_column_row_{}_bits", i)))?;

        while row_bits.len() % 8 > 0 {
            row_bits.push(Boolean::Constant(false));
        }

        bits.extend(
            row_bits
                .chunks(8)
                .flat_map(|chunk| chunk.iter().rev())
                .cloned(),
        );
    }

    let alloc_bits = sha256_circuit(cs.namespace(|| "hash"), &bits[..])?;
    let fr = if alloc_bits[0].get_value().is_some() {
        let be_bits = alloc_bits
            .iter()
            .map(|v| v.get_value().ok_or(SynthesisError::AssignmentMissing))
            .collect::<Result<Vec<bool>, SynthesisError>>()?;

        let le_bits = be_bits
            .chunks(8)
            .flat_map(|chunk| chunk.iter().rev())
            .copied()
            .take(E::Fr::CAPACITY as usize)
            .collect::<Vec<bool>>();

        Ok(multipack::compute_multipacking::<E>(&le_bits)[0])
    } else {
        Err(SynthesisError::AssignmentMissing)
    };

    num::AllocatedNum::<E>::alloc(cs.namespace(|| "result_num"), || fr)
}

#[cfg(test)]
mod tests {
    use super::*;

    use bellperson::ConstraintSystem;
    use ff::Field;
    use paired::bls12_381::{Bls12, Fr};
    use rand::SeedableRng;
    use rand_xorshift::XorShiftRng;

    use crate::circuit::test::TestConstraintSystem;
    use crate::crypto::pedersen::JJ_PARAMS;
    use crate::fr32::fr_into_bytes;
    use crate::hasher::{pedersen::*, HashFunction};
    use crate::stacked::hash::hash_single_column as vanilla_hash_single_column;

    #[test]
    fn test_hash2_circuit() {
        let rng = &mut XorShiftRng::from_seed(crate::TEST_SEED);

        for _ in 0..10 {
            let mut cs = TestConstraintSystem::<Bls12>::new();

            let a = Fr::random(rng);
            let b = Fr::random(rng);
            let a_bytes = fr_into_bytes::<Bls12>(&a);
            let b_bytes = fr_into_bytes::<Bls12>(&b);

            let a_num = {
                let cs = cs.namespace(|| "a");
                num::AllocatedNum::alloc(cs, || Ok(a)).unwrap()
            };

            let b_num = {
                let cs = cs.namespace(|| "b");
                num::AllocatedNum::alloc(cs, || Ok(b)).unwrap()
            };

            let out = PedersenFunction::hash_leaf_circuit(
                cs.namespace(|| "hash2"),
                &JJ_PARAMS,
                None,
                &a_num,
                &b_num,
            )
            .expect("hash2 function failed");

            assert!(cs.is_satisfied(), "constraints not satisfied");
            assert_eq!(cs.num_constraints(), 1374);

            let expected: Fr = PedersenFunction::hash2(&a_bytes, &b_bytes).into();

            assert_eq!(
                expected,
                out.get_value().unwrap(),
                "circuit and non circuit do not match"
            );
        }
    }

    #[test]
    fn test_hash_single_column_circuit() {
        let rng = &mut XorShiftRng::from_seed(crate::TEST_SEED);

        for _ in 0..1 {
            let mut cs = TestConstraintSystem::<Bls12>::new();

            let a = Fr::random(rng);
            let b = Fr::random(rng);
            let a_bytes = fr_into_bytes::<Bls12>(&a);
            let b_bytes = fr_into_bytes::<Bls12>(&b);

            let out = hash_single_column(
                cs.namespace(|| "hash_single_column"),
                &JJ_PARAMS,
                &[Some(a), Some(b)],
            )
            .expect("hash_single_column function failed");

            assert!(cs.is_satisfied(), "constraints not satisfied");
            assert_eq!(cs.num_constraints(), 45_378);

            let expected: Fr = vanilla_hash_single_column(&[a_bytes, b_bytes]).into();

            assert_eq!(
                expected,
                out.get_value().unwrap(),
                "circuit and non circuit do not match"
            );
        }
    }
}
